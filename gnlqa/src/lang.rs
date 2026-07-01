//! Multilingual support (C19). Resolve the question's language (from the LLM's
//! intent tag, with a script-based fallback) and produce a "respond in X"
//! instruction so LLM-generated answers — grounded explanations, GraphRAG,
//! abstention — come back in the user's language. Structured SPARQL results stay
//! language-neutral; entity linking gains multilingual label coverage in `link`.

/// Resolve the effective language code: prefer the LLM-tagged `intent_lang` when
/// it's a plausible ISO code, else a script-based guess from the text, else "en".
pub fn resolve_lang(intent_lang: &str, question: &str) -> String {
    let tagged = intent_lang.trim().to_lowercase();
    // Accept a 2–3 letter code, optionally with a region ("zh", "en-US").
    let code = tagged.split(['-', '_']).next().unwrap_or("");
    if is_iso_code(code) {
        return code.to_string();
    }
    detect_script_lang(question).unwrap_or("en").to_string()
}

fn is_iso_code(s: &str) -> bool {
    (2..=3).contains(&s.len()) && s.chars().all(|c| c.is_ascii_lowercase())
}

/// A coarse script-based language guess (fallback only). Returns `None` for Latin
/// script (characters alone can't tell en/es/fr apart).
pub fn detect_script_lang(text: &str) -> Option<&'static str> {
    let (mut han, mut kana, mut hangul, mut cyr, mut arab, mut total) = (0, 0, 0, 0, 0, 0usize);
    for c in text.chars() {
        // Only letters count toward a script — this excludes CJK punctuation
        // (e.g. the katakana middle dot U+30FB, inside the kana block but not
        // alphabetic) so a Latin string with a stray `・` isn't misread as ja.
        if !c.is_alphabetic() {
            continue;
        }
        total += 1;
        match c {
            '\u{3040}'..='\u{30FF}' => kana += 1,   // Hiragana + Katakana
            '\u{AC00}'..='\u{D7AF}' => hangul += 1,  // Hangul syllables
            '\u{4E00}'..='\u{9FFF}' => han += 1,     // CJK unified ideographs
            '\u{0400}'..='\u{04FF}' => cyr += 1,     // Cyrillic
            '\u{0600}'..='\u{06FF}' => arab += 1,    // Arabic
            _ => {}
        }
    }
    if total == 0 {
        return None;
    }
    // Kana ⇒ Japanese (Han alone is ambiguous between zh/ja); Hangul ⇒ Korean.
    if kana > 0 {
        Some("ja")
    } else if hangul > 0 {
        Some("ko")
    } else if han > 0 {
        Some("zh")
    } else if cyr > 0 {
        Some("ru")
    } else if arab > 0 {
        Some("ar")
    } else {
        None
    }
}

/// English name for a language code (for prompt instructions); falls back to the
/// code itself for anything unlisted.
pub fn language_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "zh" => "Chinese",
        "ja" => "Japanese",
        "ko" => "Korean",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "it" => "Italian",
        "pt" => "Portuguese",
        "ru" => "Russian",
        "ar" => "Arabic",
        "hi" => "Hindi",
        "nl" => "Dutch",
        "sv" => "Swedish",
        "pl" => "Polish",
        "tr" => "Turkish",
        other => other,
    }
}

/// A "respond in language X" instruction to append to an LLM prompt. Empty for
/// English (the default), to avoid noise on the common path.
pub fn lang_instruction(code: &str) -> String {
    if code == "en" {
        String::new()
    } else {
        format!(" Respond in {}.", language_name(code))
    }
}

/// A localized abstention message for common languages; English fallback.
pub fn abstain_message(code: &str) -> &'static str {
    match code {
        "zh" => "我没有足够的把握根据现有数据回答这个问题。",
        "ja" => "データからこの質問に確信を持って答えることができません。",
        "ko" => "데이터만으로는 이 질문에 확신을 갖고 답할 수 없습니다.",
        "es" => "No tengo suficiente confianza para responder esto con los datos.",
        "fr" => "Je ne suis pas assez sûr pour répondre à partir des données.",
        "de" => "Ich bin nicht sicher genug, um dies aus den Daten zu beantworten.",
        "it" => "Non ho abbastanza certezza per rispondere in base ai dati.",
        "pt" => "Não tenho confiança suficiente para responder isto com os dados.",
        "nl" => "Ik ben er niet zeker genoeg van om dit uit de gegevens te beantwoorden.",
        "ru" => "У меня недостаточно уверенности, чтобы ответить на основе данных.",
        "ar" => "لست واثقًا بما يكفي للإجابة عن هذا من البيانات.",
        "hi" => "मेरे पास इस डेटा से इसका उत्तर देने के लिए पर्याप्त निश्चितता नहीं है।",
        "tr" => "Bunu verilerden yanıtlayacak kadar emin değilim.",
        _ => "I'm not confident enough to answer this from the data.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_valid_intent_tag() {
        assert_eq!(resolve_lang("es", "cualquier cosa"), "es");
        assert_eq!(resolve_lang("en-US", "whatever"), "en"); // region stripped
        assert_eq!(resolve_lang("ZH", "随便"), "zh"); // case-normalized
    }

    #[test]
    fn resolve_falls_back_to_script_then_english() {
        // empty/garbage tag → script detection
        assert_eq!(resolve_lang("", "东京的人口是多少？"), "zh");
        assert_eq!(resolve_lang("!!", "東京はどこですか"), "ja"); // kana present
        assert_eq!(resolve_lang("", "Where is Tokyo?"), "en"); // Latin → default
    }

    #[test]
    fn script_detection_distinguishes_scripts() {
        assert_eq!(detect_script_lang("これは日本語"), Some("ja")); // kana wins over han
        assert_eq!(detect_script_lang("这是中文"), Some("zh"));
        assert_eq!(detect_script_lang("한국어입니다"), Some("ko"));
        assert_eq!(detect_script_lang("Это русский"), Some("ru"));
        assert_eq!(detect_script_lang("hello world"), None);
        assert_eq!(detect_script_lang("123 !!!"), None); // no letters
        // stray CJK punctuation (katakana middle dot) in Latin text ≠ Japanese
        assert_eq!(detect_script_lang("A・B testing"), None);
    }

    #[test]
    fn instruction_empty_for_english_else_named() {
        assert_eq!(lang_instruction("en"), "");
        assert_eq!(lang_instruction("zh"), " Respond in Chinese.");
        assert_eq!(lang_instruction("xx"), " Respond in xx."); // unknown → code
    }

    #[test]
    fn abstain_localized_with_english_fallback() {
        assert!(abstain_message("zh").contains("把握"));
        assert_eq!(abstain_message("en"), "I'm not confident enough to answer this from the data.");
        assert_eq!(abstain_message("qq"), "I'm not confident enough to answer this from the data.");
    }
}
