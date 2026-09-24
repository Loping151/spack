use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};

pub type Messages = BTreeMap<String, String>;
struct Catalog {
    id: &'static str,
    aliases: &'static [&'static str],
    source: &'static str,
    messages: OnceLock<Messages>,
}
static CATALOGS: [Catalog; 2] = [
    Catalog {
        id: "en",
        aliases: &["en", "en-us", "en-gb"],
        source: include_str!("../locales/en.json"),
        messages: OnceLock::new(),
    },
    Catalog {
        id: "zh-CN",
        aliases: &["zh", "zh-cn", "zh-hans", "zh-sg"],
        source: include_str!("../locales/zh-CN.json"),
        messages: OnceLock::new(),
    },
];

fn catalog(lang: &str) -> &'static Catalog {
    let tag = lang.replace('_', "-").to_ascii_lowercase();
    CATALOGS
        .iter()
        .find(|entry| entry.aliases.contains(&tag.as_str()) || entry.id.eq_ignore_ascii_case(&tag))
        .unwrap_or(&CATALOGS[0])
}
fn values(entry: &'static Catalog) -> &'static Messages {
    entry.messages.get_or_init(|| {
        serde_json::from_str(entry.source).expect("invalid bundled translation catalog")
    })
}
pub fn messages(lang: &str) -> Messages {
    let mut result = values(&CATALOGS[0]).clone();
    result.extend(values(catalog(lang)).clone());
    result
}

#[derive(Serialize, Deserialize)]
struct Message {
    code: String,
    #[serde(default)]
    args: Vec<String>,
}
pub fn message(code: &str, args: &[String]) -> String {
    serde_json::to_string(&Message {
        code: code.into(),
        args: args.to_vec(),
    })
    .expect("message contains only strings")
}

pub fn normalize_error(error: String) -> String {
    if values(&CATALOGS[0]).contains_key(&error) || serde_json::from_str::<Message>(&error).is_ok()
    {
        return error;
    }
    if let Some((code, _)) = values(&CATALOGS[0])
        .iter()
        .find(|(code, text)| code.starts_with("codec.") && text.eq_ignore_ascii_case(&error))
    {
        return code.clone();
    }
    message("error.system", &[error])
}

pub fn render(lang: &str, code: &str, args: &[String]) -> String {
    let template = values(catalog(lang))
        .get(code)
        .or_else(|| values(&CATALOGS[0]).get(code))
        .map(String::as_str)
        .unwrap_or(code);
    let mut output = String::with_capacity(template.len());
    let mut remainder = template;
    while let Some(start) = remainder.find('{') {
        output.push_str(&remainder[..start]);
        remainder = &remainder[start..];
        if let Some(end) = remainder.find('}') {
            if let Ok(index) = remainder[1..end].parse::<usize>() {
                if let Some(value) = args.get(index) {
                    output.push_str(value);
                    remainder = &remainder[end + 1..];
                    continue;
                }
            }
        }
        output.push('{');
        remainder = &remainder[1..];
    }
    output.push_str(remainder);
    output
}
pub fn render_error(lang: &str, error: &str) -> String {
    let normalized = normalize_error(error.into());
    match serde_json::from_str::<Message>(&normalized) {
        Ok(message) => render(lang, &message.code, &message.args),
        Err(_) => render(lang, &normalized, &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_have_matching_keys_and_parameters() {
        let english = values(&CATALOGS[0]);
        for entry in &CATALOGS {
            let localized = values(entry);
            assert_eq!(
                english.keys().collect::<Vec<_>>(),
                localized.keys().collect::<Vec<_>>()
            );
            for (key, text) in english {
                for index in 0..10 {
                    let token = format!("{{{index}}}");
                    assert_eq!(
                        text.contains(&token),
                        localized[key].contains(&token),
                        "{key}"
                    );
                }
            }
        }
    }

    #[test]
    fn language_fallback_and_path_parameters_are_stable() {
        assert_eq!(render("fr", "error.cancelled", &[]), "Operation cancelled");
        assert_eq!(render("zh_CN", "error.cancelled", &[]), "操作已取消");
        let encoded = message("error.path_case_conflict", &["{1}/a".into(), "B".into()]);
        assert_eq!(
            render_error("en", &encoded),
            "Folder paths differ only by case: {1}/a / B"
        );
        assert_eq!(
            render_error("en", "some external diagnostic"),
            "System error: some external diagnostic"
        );
    }
}
