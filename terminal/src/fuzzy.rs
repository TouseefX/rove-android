//! Small, Unicode-safe ranking for the already downloaded game results.
pub fn match_text(text: &str, query: &str) -> Option<(i32, Vec<usize>)> {
    let original: Vec<char> = text.chars().collect();
    let hay: Vec<(char, usize)> = original
        .iter()
        .enumerate()
        .flat_map(|(i, c)| c.to_lowercase().map(move |c| (c, i)))
        .collect();
    let needle: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if needle.is_empty() {
        return Some((0, vec![]));
    }
    let mut pos = 0;
    let mut score = 0;
    let mut matched = Vec::new();
    for c in needle {
        let offset = hay[pos..].iter().position(|(h, _)| *h == c)?;
        let index = hay[pos + offset].1;
        score += 10;
        if index == 0 || !original[index - 1].is_alphanumeric() {
            score += 18;
        }
        if matched.last().is_some_and(|last| *last + 1 == index) {
            score += 24;
        }
        score -= offset.min(30) as i32;
        matched.push(index);
        pos += offset + 1;
    }
    matched.dedup();
    Some((score, matched))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranks_contiguous_words_above_scattered_letters() {
        assert!(
            match_text("Obby", "obby").unwrap().0
                > match_text("Old blue boat yard", "obby").unwrap().0
        );
        assert!(match_text("Natural Disaster Survival", "nds").is_some());
        assert!(match_text("Natural Disaster Survival", "xyz").is_none());
    }
    #[test]
    fn highlights_original_unicode_character_positions() {
        let (_, positions) = match_text("🦀 İSTANBUL café", "istan").unwrap();
        assert_eq!(positions, vec![2, 3, 4, 5, 6]);
        assert!(match_text("世界", "世界").is_some());
    }
}
