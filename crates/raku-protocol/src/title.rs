//! Presentation of automatically generated session titles.

use unicode_properties::UnicodeEmoji;
use unicode_segmentation::UnicodeSegmentation;

/// Remove decorative leading emoji and Nerd Font icons without allocating or
/// touching the rest of the title. Consume whole graphemes so joined emoji,
/// skin tones, flags, and variation selectors cannot leave invisible fragments.
pub fn without_leading_icons(title: &str) -> &str {
    let mut title = title.trim();
    while let Some(first) = title.chars().next() {
        let private_use = matches!(
            first,
            '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..='\u{ffffd}' | '\u{100000}'..='\u{10fffd}'
        );
        if !private_use && !first.is_emoji_char() {
            break;
        }
        let icon = title.graphemes(true).next().unwrap();
        // Digits, '#' and '*' have the Emoji property even in ordinary text.
        // Only their keycap sequences are icons; honor explicit text selectors.
        if !private_use
            && (icon.contains('\u{fe0e}') || (first.is_ascii() && !icon.ends_with('\u{20e3}')))
        {
            break;
        }
        title = title[icon.len()..].trim_start();
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_leading_emoji_sequences_and_private_use_icons() {
        for prefix in [
            "🤖 ",
            "🖥️ ",
            "👩🏽‍💻 ",
            "🇨🇳 ",
            "1️⃣ ",
            "#️⃣ ",
            "*️⃣ ",
            "🏴\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f} ",
            "\u{ea76} ",
            "\u{f0000} ",
            "\u{100000} ",
            " 🧠🛠️  ",
            "🏷️",
        ] {
            assert_eq!(
                without_leading_icons(&format!("{prefix}MODELS: 排查模型安装  ")),
                "MODELS: 排查模型安装",
                "{prefix:?}"
            );
        }
        assert_eq!(without_leading_icons(" 🤖 🖥️ "), "");
        assert_eq!(without_leading_icons(""), "");
    }

    #[test]
    fn preserves_words_numbers_punctuation_and_emoji_in_the_body() {
        for title in [
            "按提示语言自动生成标题",
            "3 ways to fix titles",
            "#123 Fix title events",
            "* Handle title events",
            "C++ title handling",
            "日本語のタイトル",
            "제목 수정",
            "Fix 👩🏽‍💻 emoji handling 🖥️",
            "🤖\u{fe0e} Render an explicit text symbol",
        ] {
            assert_eq!(without_leading_icons(title), title);
        }
        assert_eq!(
            without_leading_icons("🤖 MODELS: Keep 🖥️ in the body"),
            "MODELS: Keep 🖥️ in the body"
        );
    }
}
