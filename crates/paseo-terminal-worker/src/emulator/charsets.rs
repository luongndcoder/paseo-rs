//! xterm.js national replacement charsets (`Charsets.ts`), generated from @xterm/headless 6.0.0.
//! Charset `B` (US ASCII) is the default and has no mapping.

/// Maps `ch` through the charset designated by `flag` (the final byte of `ESC ( X`).
pub fn map(flag: u8, ch: char) -> Option<char> {
    match flag {
        b'0' => match ch {
            '`' => Some('◆'),
            'a' => Some('▒'),
            'b' => Some('␉'),
            'c' => Some('␌'),
            'd' => Some('␍'),
            'e' => Some('␊'),
            'f' => Some('°'),
            'g' => Some('±'),
            'h' => Some('␤'),
            'i' => Some('␋'),
            'j' => Some('┘'),
            'k' => Some('┐'),
            'l' => Some('┌'),
            'm' => Some('└'),
            'n' => Some('┼'),
            'o' => Some('⎺'),
            'p' => Some('⎻'),
            'q' => Some('─'),
            'r' => Some('⎼'),
            's' => Some('⎽'),
            't' => Some('├'),
            'u' => Some('┤'),
            'v' => Some('┴'),
            'w' => Some('┬'),
            'x' => Some('│'),
            'y' => Some('≤'),
            'z' => Some('≥'),
            '{' => Some('π'),
            '|' => Some('≠'),
            '}' => Some('£'),
            '~' => Some('·'),
            _ => None,
        },
        b'4' => match ch {
            '#' => Some('£'),
            '@' => Some('¾'),
            '[' => Some('i'),
            '\\' => Some('½'),
            ']' => Some('|'),
            '{' => Some('¨'),
            '|' => Some('f'),
            '}' => Some('¼'),
            '~' => Some('´'),
            _ => None,
        },
        b'5' | b'C' => match ch {
            '[' => Some('Ä'),
            '\\' => Some('Ö'),
            ']' => Some('Å'),
            '^' => Some('Ü'),
            '`' => Some('é'),
            '{' => Some('ä'),
            '|' => Some('ö'),
            '}' => Some('å'),
            '~' => Some('ü'),
            _ => None,
        },
        b'6' | b'E' => match ch {
            '@' => Some('Ä'),
            '[' => Some('Æ'),
            '\\' => Some('Ø'),
            ']' => Some('Å'),
            '^' => Some('Ü'),
            '`' => Some('ä'),
            '{' => Some('æ'),
            '|' => Some('ø'),
            '}' => Some('å'),
            '~' => Some('ü'),
            _ => None,
        },
        b'7' | b'H' => match ch {
            '@' => Some('É'),
            '[' => Some('Ä'),
            '\\' => Some('Ö'),
            ']' => Some('Å'),
            '^' => Some('Ü'),
            '`' => Some('é'),
            '{' => Some('ä'),
            '|' => Some('ö'),
            '}' => Some('å'),
            '~' => Some('ü'),
            _ => None,
        },
        b'A' => match ch {
            '#' => Some('£'),
            _ => None,
        },
        b'R' => match ch {
            '#' => Some('£'),
            '@' => Some('à'),
            '[' => Some('°'),
            '\\' => Some('ç'),
            ']' => Some('§'),
            '{' => Some('é'),
            '|' => Some('ù'),
            '}' => Some('è'),
            '~' => Some('¨'),
            _ => None,
        },
        b'Q' => match ch {
            '@' => Some('à'),
            '[' => Some('â'),
            '\\' => Some('ç'),
            ']' => Some('ê'),
            '^' => Some('î'),
            '`' => Some('ô'),
            '{' => Some('é'),
            '|' => Some('ù'),
            '}' => Some('è'),
            '~' => Some('û'),
            _ => None,
        },
        b'K' => match ch {
            '@' => Some('§'),
            '[' => Some('Ä'),
            '\\' => Some('Ö'),
            ']' => Some('Ü'),
            '{' => Some('ä'),
            '|' => Some('ö'),
            '}' => Some('ü'),
            '~' => Some('ß'),
            _ => None,
        },
        b'Y' => match ch {
            '#' => Some('£'),
            '@' => Some('§'),
            '[' => Some('°'),
            '\\' => Some('ç'),
            ']' => Some('é'),
            '`' => Some('ù'),
            '{' => Some('à'),
            '|' => Some('ò'),
            '}' => Some('è'),
            '~' => Some('ì'),
            _ => None,
        },
        b'Z' => match ch {
            '#' => Some('£'),
            '@' => Some('§'),
            '[' => Some('¡'),
            '\\' => Some('Ñ'),
            ']' => Some('¿'),
            '{' => Some('°'),
            '|' => Some('ñ'),
            '}' => Some('ç'),
            _ => None,
        },
        b'=' => match ch {
            '#' => Some('ù'),
            '@' => Some('à'),
            '[' => Some('é'),
            '\\' => Some('ç'),
            ']' => Some('ê'),
            '^' => Some('î'),
            '_' => Some('è'),
            '`' => Some('ô'),
            '{' => Some('ä'),
            '|' => Some('ö'),
            '}' => Some('ü'),
            '~' => Some('û'),
            _ => None,
        },
        _ => None,
    }
}

/// Charset designators known to xterm.js (`CHARSETS` keys); any other final byte is ignored.
pub fn is_known(flag: u8) -> bool {
    matches!(
        flag,
        b'0' | b'4'
            | b'5'
            | b'6'
            | b'7'
            | b'A'
            | b'B'
            | b'C'
            | b'R'
            | b'Q'
            | b'K'
            | b'Y'
            | b'E'
            | b'Z'
            | b'H'
            | b'='
    )
}
