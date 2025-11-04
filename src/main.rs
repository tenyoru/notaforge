mod anki;
mod card_template;
mod config;
mod vocab_service;
use anki::*;
use ankiconnect_rs::{
    AnkiClient, DuplicateScope, NoteBuilder,
    builders::{Query, QueryBuilder},
};
use anyhow::{Result, anyhow};
use card_template::{CardFields, CardTemplate, SimpleCard, VocabularyCard};
use clap::{Parser, ValueEnum};
use std::{env, path::PathBuf};
use vocab_service::build_vocabulary_card;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the configuration file (TOML)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Name of the Anki deck to use
    #[arg(short, long)]
    deck: Option<String>,

    /// Name of the Anki model to use (default: Basic)
    #[arg(short, long)]
    model: Option<String>,

    /// Card template to use when generating fields
    #[arg(short, long, value_enum)]
    template: Option<TemplateKind>,

    /// Term to build a card for
    #[arg(short = 'w', long)]
    term: String,

    /// Source language code used for translation lookups
    #[arg(long)]
    source_lang: Option<String>,

    /// Target language code used for translation lookups
    #[arg(long)]
    target_lang: Option<String>,

    /// Maximum number of retries for translation API calls
    #[arg(long, default_value_t = 2)]
    translate_retries: u32,

    /// Base backoff in milliseconds for translation retries
    #[arg(long, default_value_t = 500)]
    translate_backoff_ms: u64,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum TemplateKind {
    Vocabulary,
    Simple,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let config_path = args
        .config
        .clone()
        .or_else(|| env::var_os("NOTAFORGE_CONFIG").map(PathBuf::from))
        .or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .map(|base| base.join("notaforge/config.toml"))
        })
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".config/notaforge/config.toml"))
        })
        .unwrap_or_else(|| PathBuf::from("notaforge.toml"));

    let config = config::load(&config_path)?;

    let deck_name = args
        .deck
        .clone()
        .or_else(|| config.deck.clone())
        .ok_or_else(|| anyhow!("Deck must be provided via CLI or config"))?;

    let model_name = args
        .model
        .clone()
        .or_else(|| config.model.clone())
        .ok_or_else(|| anyhow!("Model must be provided via CLI or config"))?;

    let template_kind = match args.template {
        Some(kind) => kind,
        None => match config.template.as_deref() {
            Some(name) => TemplateKind::from_str(name, true)
                .map_err(|_| anyhow!("Invalid template '{}' in config", name))?,
            None => TemplateKind::Vocabulary,
        },
    };

    let source_lang = args
        .source_lang
        .clone()
        .or_else(|| config.source_lang.clone())
        .unwrap_or_else(|| "en".to_string());

    let target_lang = args
        .target_lang
        .clone()
        .or_else(|| config.target_lang.clone())
        .unwrap_or_else(|| "ru".to_string());

    let translate_retries = args
        .translate_retries
        .max(config.translate_retries.unwrap_or(args.translate_retries));
    let translate_backoff_ms = args.translate_backoff_ms.max(
        config
            .translate_backoff_ms
            .unwrap_or(args.translate_backoff_ms),
    );

    let translation_bases = if !config.translation_bases.is_empty() {
        config.translation_bases.clone()
    } else if let Some(base) = config.legacy_translation_base.clone() {
        vec![base]
    } else {
        Vec::new()
    };

    let client = AnkiClient::new();
    let deck = find_deck(&client, &deck_name)?;
    let model = find_model(&client, &model_name)?;

    let front_field = get_model_field(&model, "Front")?;
    let back_field = get_model_field(&model, "Back")?;

    let term_tag = build_term_tag(&args.term);
    let duplicate_query = build_duplicate_query(deck.name(), &term_tag);

    if !client.cards().find(&duplicate_query)?.is_empty() {
        println!(
            "Note for term '{}' already exists in deck '{}'; skipping.",
            args.term,
            deck.name()
        );
        return Ok(());
    }

    let http_client = reqwest::Client::new();
    let vocabulary_card = build_vocabulary_card(
        &http_client,
        &args.term,
        &source_lang,
        &target_lang,
        &translation_bases,
        translate_retries,
        translate_backoff_ms,
    )
    .await?;

    let mut fields = match template_kind {
        TemplateKind::Vocabulary => vocabulary_card.render(),
        TemplateKind::Simple => render_simple_fields(&vocabulary_card),
    };

    if !fields.tags.iter().any(|tag| tag == &term_tag) {
        fields.tags.push(term_tag.clone());
    }

    for tag in &config.extra_tags {
        if !fields.tags.iter().any(|existing| existing == tag) {
            fields.tags.push(tag.clone());
        }
    }

    let mut builder = NoteBuilder::new(model.clone())
        .with_field_raw(front_field, &fields.front)
        .with_field_raw(back_field, &fields.back);

    for tag in &fields.tags {
        builder = builder.with_tag(tag);
    }

    let note = builder.build()?;

    // Add the note to the first deck
    match client
        .cards()
        .add_note(&deck, note, false, Some(DuplicateScope::Deck))
    {
        Ok(note_id) => {
            println!("Added note with ID: {}", note_id.value());
            Ok(())
        }
        Err(err)
            if err.to_string().to_lowercase().contains("duplicate note")
                || err.to_string().to_lowercase().contains("duplicate") =>
        {
            println!(
                "Note for term '{}' already exists in deck '{}'; skipping.",
                args.term,
                deck.name()
            );
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

fn render_simple_fields(card: &VocabularyCard) -> CardFields {
    let mut tags = card.extra_tags.clone();
    if !card.part_of_speech.is_empty() {
        tags.push(card.part_of_speech.clone());
    }

    let synonyms_block = if card.translation_synonyms.is_empty() {
        String::new()
    } else {
        format!(
            "<div style=\"margin-top:0.6em; color:#5e84c1;\">{}</div>",
            card.translation_synonyms
        )
    };

    SimpleCard {
        front: format!("<b>{}</b>", card.term),
        back: format!(
            concat!(
                "<div style=\"font-size:1.2em;\">{translation}</div>",
                "{synonyms}",
                "<div style=\"margin-top:0.8em; color:#666;\">{usage}</div>",
            ),
            translation = card.translation_heading,
            synonyms = synonyms_block,
            usage = card.translation_usage,
        ),
        tags,
    }
    .render()
}

fn build_term_tag(term: &str) -> String {
    let mut slug = String::with_capacity(term.len());
    let mut last_was_sep = false;

    for c in term.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if c.is_whitespace() || matches!(c, '-' | '_' | ':' | '/' | '\\') {
            if !last_was_sep && !slug.is_empty() {
                slug.push('_');
            }
            last_was_sep = true;
        } else {
            if !last_was_sep && !slug.is_empty() {
                slug.push('_');
            }
            last_was_sep = true;
        }
    }

    if slug.ends_with('_') {
        slug.pop();
    }

    if slug.is_empty() {
        slug.push_str("term");
    }

    format!("term:{}", slug)
}

fn build_duplicate_query(deck_name: &str, term_tag: &str) -> Query {
    QueryBuilder::new()
        .in_deck(deck_name)
        .and()
        .has_tag("auto-generated")
        .and()
        .has_tag(term_tag)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_tag_slugifies_special_chars() {
        assert_eq!(build_term_tag("taken aback"), "term:taken_aback");
        assert_eq!(build_term_tag("  Weird-term?! "), "term:weird_term");
    }

    #[test]
    fn duplicate_query_matches_expected_structure() {
        let query = build_duplicate_query("My Deck", "term:word");
        assert_eq!(
            query.as_str(),
            "deck:\"My Deck\" tag:auto\\-generated tag:term\\:word"
        );
    }
}
