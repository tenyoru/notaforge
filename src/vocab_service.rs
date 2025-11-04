use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow};
use futures::future::join_all;
use reqwest::Client;
use serde::Deserialize;

use crate::card_template::{ExampleSentence, VocabularyCard};

const DICTIONARY_ENDPOINT: &str = "https://api.dictionaryapi.dev/api/v2/entries/en/";
const DATAMUSE_ENDPOINT: &str = "https://api.datamuse.com/words";
const DEFAULT_TRANSLATE_BASES: &[&str] = &[
    "https://lingva.ml/api/v1",
    "https://lingva.garudalinux.org/api/v1",
    "https://translate.plausible.stream/api/v1",
];

pub async fn build_vocabulary_card(
    client: &Client,
    term: &str,
    source_lang: &str,
    target_lang: &str,
    translate_bases: &[String],
    translate_retries: u32,
    translate_backoff_ms: u64,
) -> Result<VocabularyCard> {
    let (dictionary_res, datamuse_res) = tokio::join!(
        fetch_dictionary_entry(client, term),
        fetch_datamuse_synonyms(client, term)
    );

    let dictionary = dictionary_res.unwrap_or_default();
    let mut synonyms_set: BTreeSet<String> = dictionary.synonyms.iter().cloned().collect();
    let datamuse_synonyms = datamuse_res.unwrap_or_default();
    synonyms_set.extend(datamuse_synonyms);
    let synonyms: Vec<String> = synonyms_set.into_iter().collect();

    let part_of_speech = dictionary.part_of_speech.unwrap_or_default();
    let pronunciation = dictionary.pronunciation.unwrap_or_default();

    let base_candidates: Vec<String> = if translate_bases.is_empty() {
        DEFAULT_TRANSLATE_BASES
            .iter()
            .map(|base| base.to_string())
            .collect()
    } else {
        translate_bases.iter().cloned().collect()
    };
    let base_slice = &base_candidates;

    let synonyms_joined = synonyms.join(", ");
    let translated_synonyms = if synonyms_joined.is_empty() {
        String::new()
    } else {
        let futures = synonyms.iter().map(|synonym| {
            translate_text(
                client,
                synonym,
                source_lang,
                target_lang,
                base_slice,
                translate_retries,
                translate_backoff_ms,
            )
        });
        let results = join_all(futures).await;

        let mut translated = Vec::with_capacity(synonyms.len());
        for (original, result) in synonyms.iter().zip(results) {
            match result {
                Ok(value) if !value.trim().is_empty() => translated.push(value),
                _ => translated.push(original.clone()),
            }
        }
        translated.join(", ")
    };

    let definition_text = dictionary
        .definition
        .unwrap_or_else(|| format!("No definition found for {term}."));

    let (translation_res, usage_res) = tokio::join!(
        translate_text(
            client,
            term,
            source_lang,
            target_lang,
            base_slice,
            translate_retries,
            translate_backoff_ms,
        ),
        translate_text(
            client,
            &definition_text,
            source_lang,
            target_lang,
            base_slice,
            translate_retries,
            translate_backoff_ms,
        )
    );

    let translation = match translation_res {
        Ok(value) if !value.trim().is_empty() => value,
        Ok(_) => term.to_string(),
        Err(_) => term.to_string(),
    };

    let translated_usage = match usage_res {
        Ok(value) if !value.trim().is_empty() => value,
        _ => definition_text.clone(),
    };

    let example_sentence = dictionary
        .example
        .unwrap_or_else(|| format!("This sentence uses the word {term}."));

    let highlight = if example_sentence.contains(term) {
        term.to_string()
    } else {
        String::new()
    };

    Ok(VocabularyCard {
        term: term.to_string(),
        pronunciation,
        part_of_speech,
        example: ExampleSentence {
            sentence: example_sentence,
            highlight,
        },
        translation_heading: translation,
        translation_synonyms: translated_synonyms,
        translation_usage: translated_usage,
        extra_tags: vec![
            source_lang.to_string(),
            target_lang.to_string(),
            "auto-generated".to_string(),
        ],
    })
}

async fn fetch_dictionary_entry(client: &Client, term: &str) -> Result<DictionaryData> {
    let url = format!("{DICTIONARY_ENDPOINT}{term}");
    let entries: Vec<DictionaryEntry> = client
        .get(&url)
        .send()
        .await
        .context("Dictionary request failed")?
        .error_for_status()
        .context("Dictionary service returned error")?
        .json()
        .await
        .context("Dictionary response parsing failed")?;

    let entry = entries
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No dictionary entry for '{term}'"))?;

    let pronunciation = entry
        .phonetic
        .clone()
        .or_else(|| entry.phonetics.iter().find_map(|p| p.text.clone()));

    let meaning = entry
        .meanings
        .into_iter()
        .find(|meaning| !meaning.definitions.is_empty())
        .ok_or_else(|| anyhow!("Dictionary missing definitions for '{term}'"))?;

    let definitions = meaning.definitions.clone();

    let definition = definitions
        .iter()
        .find_map(|def| (!def.definition.is_empty()).then(|| def.definition.clone()));

    let example = definitions.iter().find_map(|def| def.example.clone());

    let synonyms = collect_synonyms(&definitions, meaning.synonyms);

    Ok(DictionaryData {
        pronunciation,
        part_of_speech: meaning.part_of_speech,
        definition,
        example,
        synonyms,
    })
}

async fn fetch_datamuse_synonyms(client: &Client, term: &str) -> Result<Vec<String>> {
    let response: Vec<DatamuseEntry> = client
        .get(DATAMUSE_ENDPOINT)
        .query(&[("rel_syn", term), ("max", "5")])
        .send()
        .await
        .context("Datamuse request failed")?
        .error_for_status()
        .context("Datamuse returned error")?
        .json()
        .await
        .context("Datamuse response parsing failed")?;

    Ok(response.into_iter().map(|entry| entry.word).collect())
}

async fn translate_text(
    client: &Client,
    text: &str,
    source_lang: &str,
    target_lang: &str,
    translate_bases: &[String],
    retries: u32,
    backoff_ms: u64,
) -> Result<String> {
    if text.trim().is_empty() {
        return Ok(String::new());
    }

    let base_candidates: Vec<String> = if translate_bases.is_empty() {
        DEFAULT_TRANSLATE_BASES
            .iter()
            .map(|base| base.to_string())
            .collect()
    } else {
        translate_bases.iter().cloned().collect()
    };

    for base in base_candidates {
        match translate_with_base(
            client,
            text,
            source_lang,
            target_lang,
            &base,
            retries,
            backoff_ms,
        )
        .await
        {
            Ok(result) if !result.trim().is_empty() => return Ok(result),
            Ok(_) => continue,
            Err(_err) => continue,
        }
    }

    Ok(String::new())
}

async fn translate_with_base(
    client: &Client,
    text: &str,
    source_lang: &str,
    target_lang: &str,
    base: &str,
    retries: u32,
    backoff_ms: u64,
) -> Result<String> {
    if text.trim().is_empty() {
        return Ok(String::new());
    }

    #[derive(Deserialize)]
    struct LingvaResponse {
        translation: String,
    }

    let base = base.trim_end_matches('/');
    let url = format!(
        "{}/{}/{}/{}",
        base,
        source_lang,
        target_lang,
        urlencoding::encode(text)
    );

    let mut attempt = 0;
    let mut delay = backoff_ms.max(200);

    loop {
        let response = client.get(&url).send().await;
        match response {
            Ok(resp) => {
                if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    if attempt < retries {
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        attempt += 1;
                        delay = (delay as f64 * 1.5).round() as u64;
                        continue;
                    } else {
                        return Ok(String::new());
                    }
                }

                match resp.error_for_status() {
                    Ok(success) => {
                        let parsed: LingvaResponse = success
                            .json()
                            .await
                            .context("Lingva response parsing failed")?;
                        return Ok(parsed.translation);
                    }
                    Err(err) => {
                        if attempt < retries {
                            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                            attempt += 1;
                            delay = (delay as f64 * 1.5).round() as u64;
                            continue;
                        } else {
                            return Err(err).context("Lingva returned error");
                        }
                    }
                }
            }
            Err(err) => {
                if attempt < retries {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    attempt += 1;
                    delay = (delay as f64 * 1.5).round() as u64;
                    continue;
                }
                return Err(err).context("Lingva request failed");
            }
        }
    }
}

fn collect_synonyms(definitions: &[Definition], base_synonyms: Vec<String>) -> Vec<String> {
    let mut set: BTreeSet<String> = base_synonyms.into_iter().collect();
    for definition in definitions {
        for synonym in &definition.synonyms {
            set.insert(synonym.clone());
        }
    }
    set.into_iter().collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DictionaryEntry {
    phonetic: Option<String>,
    #[serde(default)]
    phonetics: Vec<Phonetic>,
    #[serde(default)]
    meanings: Vec<Meaning>,
}

#[derive(Deserialize)]
struct Phonetic {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meaning {
    #[serde(default)]
    part_of_speech: Option<String>,
    #[serde(default)]
    definitions: Vec<Definition>,
    #[serde(default)]
    synonyms: Vec<String>,
}

#[derive(Clone, Deserialize)]
struct Definition {
    definition: String,
    #[serde(default)]
    example: Option<String>,
    #[serde(default)]
    synonyms: Vec<String>,
}

#[derive(Deserialize)]
struct DatamuseEntry {
    word: String,
}

#[derive(Default)]
struct DictionaryData {
    pronunciation: Option<String>,
    part_of_speech: Option<String>,
    definition: Option<String>,
    example: Option<String>,
    synonyms: Vec<String>,
}
