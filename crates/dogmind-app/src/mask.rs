//! Keys and tokens in what a tool brought back, and in the model's own reply where it says them again, hidden where the window shows it: a command that prints the environment, a config file
//! read, a log, would otherwise put an API key on the screen (and in a screenshot of it). What the model is given is not changed here: it
//! may need the value, and the agent keeps reading secret files behind the person's approval. A key is told by its maker's prefix and its
//! length; its prefix and first four characters stay, so that it can still be told which key it is.

use std::borrow::Cow;

/// A maker's prefix, how many characters must follow it, and whether those must hold a digit (a word with a prefix like `sk-` is no key). A longer prefix comes
/// before one it begins with. (Stripe's secret and restricted keys, npm's, Docker's, Slack's of an app and of its other kinds, PyPI's and GitLab's runner and
/// deploy tokens were shown whole: a review of the app found them missing, where the README says keys are hidden.)
const KINDS: [(&str, usize, bool); 27] = [
    ("sk-ant-", 20, false),
    ("sk-proj-", 20, false),
    ("sk-", 20, true),
    ("sk_live_", 20, false),
    ("sk_test_", 20, false),
    ("rk_live_", 20, false),
    ("rk_test_", 20, false),
    ("npm_", 30, false),
    ("dckr_pat_", 20, false),
    ("xapp-", 10, false),
    ("xoxa-", 10, false),
    ("xoxr-", 10, false),
    ("xoxs-", 10, false),
    ("pypi-", 50, false),
    ("github_pat_", 20, false),
    ("ghp_", 20, false),
    ("gho_", 20, false),
    ("ghu_", 20, false),
    ("ghs_", 20, false),
    ("ghr_", 20, false),
    ("glpat-", 20, false),
    ("glrt-", 20, false),
    ("gldt-", 20, false),
    ("xoxb-", 10, false),
    ("xoxp-", 10, false),
    ("hf_", 30, false),
    ("AIza", 35, false),
];

fn key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// `text` with every key in it hidden but its prefix and first four characters; borrowed when there is none.
pub fn hide_keys(text: &str) -> Cow<'_, str> {
    let mut out = String::new();
    let mut copied = 0;
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        // (a key starts a word: `task-…` or `disk-…` holds no `sk-` key)
        let starts_word = at == 0 || !text[..at].chars().next_back().is_some_and(key_char);
        let found = starts_word.then(|| {
            KINDS.iter().find_map(|(prefix, least, digit)| {
                let body = rest.strip_prefix(prefix)?;
                let length = body.find(|c: char| !key_char(c)).unwrap_or(body.len());
                let key = &body[..length];
                (length >= *least && (!digit || key.chars().any(|c| c.is_ascii_digit()))).then_some((prefix.len(), length))
            })
        }).flatten()
            .or_else(|| aws_key(rest).filter(|_| starts_word).map(|length| (4, length - 4)))
            .or_else(|| dotted_key(rest).filter(|_| starts_word))
            .or_else(|| labelled_secret(&text[..at], rest).filter(|_| starts_word));
        match found {
            Some((prefix, length)) => {
                let shown = prefix + 4;
                out.push_str(&text[copied..at + shown]);
                out.push_str("…(hidden)");
                at += prefix + length;
                copied = at;
            }
            None => at += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// The model's reply as it comes in pieces, with the keys in it hidden as `hide_keys` hides them in a whole text: a key that is cut between two pieces is no less a key. What stands at the
/// end of what has come, if it begins like a key (or is one so far), is held back until what follows says whether it is; `finish` lets go of what is held when the reply ends. What is shown is
/// all this changes: the reply as the model wrote it is kept as it is, in the session and for the next request.
#[derive(Default)]
pub struct StreamMask {
    held: String,
    /// What was shown last ended in the middle of a word: what comes next goes on with it, and a key is not told by what begins here.
    after_word: bool,
}

/// The longest a word that may be a key is waited for: no key is longer, and a word as long as this is not one.
const LONGEST_WAIT: usize = 400;

/// The longest a run of words and dots that may be a key of parts (`dotted_key`) is waited for: a JSON Web Token runs to a thousand characters and more.
const LONGEST_DOTTED_WAIT: usize = 4000;

impl StreamMask {
    /// `piece` of the reply, in; what may be shown now, out.
    pub fn push(&mut self, piece: &str) -> String {
        self.held.push_str(piece);
        let cut = self.held.len() - waiting_tail(&self.held);
        let ready: String = self.held.drain(..cut).collect();
        self.hidden(&ready)
    }

    /// The reply has ended: whatever was held, hidden as it should be.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.held);
        self.hidden(&rest)
    }

    /// `text`, the next of the reply, with its keys hidden, in the word the last of it ended in if it did (a digit stands in front of it for that: no prefix begins with one).
    fn hidden(&mut self, text: &str) -> String {
        let Some(last) = text.chars().next_back() else { return String::new() };
        let shown = if self.after_word { hide_keys(&format!("0{text}"))[1..].to_string() } else { hide_keys(text).into_owned() };
        self.after_word = key_char(last);
        shown
    }
}

/// How many bytes at the end of `text` are a word that may still turn out to be a key: it begins as a key's prefix does, or is the beginning of one.
fn waiting_tail(text: &str) -> usize {
    let word = text.len() - text.trim_end_matches(key_char).len();
    // (a word longer than any key is not waited for)
    let word = if word > LONGEST_WAIT { 0 } else { word };
    let tail = &text[text.len() - word..];
    // (a word starts where the one before it stops: what is inside a longer word is no key's beginning)
    let starts_word = text.len() == word || !text[..text.len() - word].chars().next_back().is_some_and(key_char);
    let begins_like_a_key = !tail.is_empty() && KINDS.iter().map(|(prefix, ..)| *prefix).chain(std::iter::once("AKIA")).any(|prefix| tail.starts_with(prefix) || prefix.starts_with(tail));
    let plain = if starts_word && begins_like_a_key { word } else { 0 };
    // (a key of parts goes on past its dots: a run of words and dots that begins like one is waited for as well, longer)
    let run = text.len() - text.trim_end_matches(|c: char| key_char(c) || c == '.').len();
    let tail = &text[text.len() - run..];
    let run_starts_word = text.len() == run || !text[..text.len() - run].chars().next_back().is_some_and(key_char);
    let like_dotted = !tail.is_empty() && ["eyJ", "SG."].iter().any(|prefix| tail.starts_with(prefix) || prefix.starts_with(tail));
    let dotted = if run <= LONGEST_DOTTED_WAIT && run_starts_word && like_dotted { run } else { 0 };
    plain.max(dotted)
}

/// A key of parts with dots between at the start of `text`: a JSON Web Token (three parts of base64url, the first two `eyJ`, a JSON object's beginning) or a
/// SendGrid key (`SG.` and two parts). The length of what is shown of its beginning, and of the rest. Told by its parts, as a dot is no key's character: cut at
/// the first dot, the part after it, a token's claims and its signature, was shown.
fn dotted_key(text: &str) -> Option<(usize, usize)> {
    let part = |from: &str| from.find(|c: char| !key_char(c)).unwrap_or(from.len());
    let parts = |from: &str, count: usize, least: usize| -> Option<usize> {
        let mut at = 0;
        for index in 0..count {
            let length = part(&from[at..]);
            if length < least {
                return None;
            }
            at += length;
            if index + 1 < count {
                at += from[at..].strip_prefix('.').map(|_| 1)?;
            }
        }
        // (the token ends here: a dot and more of it would be a longer one, which this is not)
        (!from[at..].starts_with('.') || !from[at + 1..].starts_with(key_char)).then_some(at)
    };
    if text.starts_with("eyJ") {
        let length = parts(text, 3, 10)?;
        let claims = text[..length].split('.').nth(1)?;
        return claims.starts_with("eyJ").then_some((3, length - 3));
    }
    let body = text.strip_prefix("SG.")?;
    parts(body, 2, 16).map(|length| (3, length))
}

/// A secret with no maker's prefix that stands after the name it is given: an AWS secret access key, forty characters of base64 after `secret_access_key` (or
/// `SecretAccessKey`) and what may stand between (`=`, `:`, quotes, spaces). `before`: the text before it. The length of what is shown of it, and of the rest.
fn labelled_secret(before: &str, text: &str) -> Option<(usize, usize)> {
    let named = before.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '=' | ':' | '"' | '\'' | ','));
    if named.len() == before.len() {
        return None;
    }
    // (the name's own end alone, as it is written in any case: this is asked at every word of a long output)
    let mut from = named.len().saturating_sub(17);
    while !named.is_char_boundary(from) {
        from += 1;
    }
    let name = named[from..].to_ascii_lowercase();
    if !["secret_access_key", "secretaccesskey", "aws_secret_key"].iter().any(|known| name.ends_with(known)) {
        return None;
    }
    let length = text.bytes().take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'+')).count();
    (length == 40).then_some((0, 40))
}

/// An AWS access key id at the start of `text`: AKIA and sixteen capitals or digits, the word ending there. Its whole length.
fn aws_key(text: &str) -> Option<usize> {
    let body = text.strip_prefix("AKIA")?.as_bytes();
    let length = body.iter().take_while(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()).count();
    (length == 16 && !body.get(16).is_some_and(|byte| byte.is_ascii_alphanumeric())).then_some(20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_hidden_but_their_beginning() {
        let shown = hide_keys("OPENAI_API_KEY=sk-proj-abcdEFGH1234ijklMNOP5678 and token ghp_ABCDEFGHIJKLMNOPQRSTuvwxyz12 done");
        assert_eq!(shown, "OPENAI_API_KEY=sk-proj-abcd…(hidden) and token ghp_ABCD…(hidden) done");
        assert_eq!(hide_keys("aws AKIAIOSFODNN7EXAMPLE x"), "aws AKIAIOSF…(hidden) x");
        assert_eq!(hide_keys("key: sk-ant-api03-0123456789abcdefghijklmnop"), "key: sk-ant-api0…(hidden)");
    }

    /// The kinds a review found shown whole: Stripe's, npm's, Docker's, Slack's of an app, PyPI's, GitLab's runner tokens; a JSON Web Token and a SendGrid key
    /// with their dots, all of them and not their first part alone; an AWS secret access key after its name.
    #[test]
    fn more_kinds_of_keys_are_hidden_too() {
        // (each made-up key is put together from pieces, so that the source has no string in a key's form for a scanner of secrets to take for one)
        for (said, shown) in [
            (concat!("STRIPE=sk_", "live_", "51HqLyjWDarjtT1zdp7dcXYZ01234567 ok"), "STRIPE=sk_live_51Hq…(hidden) ok"),
            (concat!("rk_", "test_", "51HqLyjWDarjtT1zdp7dc"), "rk_test_51Hq…(hidden)"),
            (concat!("//registry.npmjs.org/:_authToken=npm", "_", "aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5"), "//registry.npmjs.org/:_authToken=npm_aB3d…(hidden)"),
            (concat!("dckr_", "pat_", "2b3e36beeb386dfdaacb9b46004"), "dckr_pat_2b3e…(hidden)"),
            (concat!("xapp", "-1-", "A0123456789-0123456789012-abcdef"), "xapp-1-A0…(hidden)"),
            (concat!("password = pypi", "-", "AgEIcHlwaS5vcmcCJGUwYzI0ZjFhLTY0NmYtNDU0Zi04ZjM1LTk2ZDY0NjRmNjNhMQ"), "password = pypi-AgEI…(hidden)"),
            (concat!("glrt", "-", "AbCdEfGhIjKlMnOpQrSt12"), "glrt-AbCd…(hidden)"),
            (concat!("Authorization: Bearer ", jwt!(), "."), "Authorization: Bearer eyJhbGc…(hidden)."),
            (concat!("SENDGRID=", sendgrid!(), "lmnopq end"), "SENDGRID=SG.abcd…(hidden) end"),
            (concat!("aws_secret_access_key = ", aws_secret!()), "aws_secret_access_key = wJal…(hidden)"),
            (concat!("\"SecretAccessKey\": \"", aws_secret!(), "\""), "\"SecretAccessKey\": \"wJal…(hidden)\""),
        ] {
            assert_eq!(hide_keys(said), shown, "{said}");
        }
        for text in ["eyJhbGciOiJIUzI1NiJ9 alone", "eyJhbGciOiJIUzI1NiJ9.notclaims123456.abcdefghijk", "SG. Then it goes on", "e.g. this", "secret_access_key = tooshort/abc",
            concat!("my ", aws_secret!(), " has no name before it"), "npm_install_dependencies"] {
            assert!(matches!(hide_keys(text), Cow::Borrowed(_)), "{text}");
        }
    }

    /// A made-up JSON Web Token, put together from its parts.
    macro_rules! jwt {
        () => {
            concat!("eyJhbGciOiJIUzI1NiJ9", ".", "eyJzdWIiOiIxMjM0NTY3ODkwIn0", ".", "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c")
        };
    }
    use jwt;

    /// A made-up SendGrid key, put together from its parts, short of its last characters (each use adds them).
    macro_rules! sendgrid {
        () => {
            concat!("SG", ".", "abcdefghijklmnopqrstuv", ".", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk")
        };
    }
    use sendgrid;

    /// The secret of AWS's own example of a key pair, put together from pieces.
    macro_rules! aws_secret {
        () => {
            concat!("wJalrXUtnFEMI", "/K7MDENG/", "bPxRfiCYEXAMPLEKEY")
        };
    }
    use aws_secret;

    const KEY: &str = "sk-proj-abcdEFGH1234ijklMNOP5678";

    /// The reply said in pieces of any size, cut anywhere, shows what the whole of it would, and no piece of a key is shown before it is known to be one.
    #[test]
    fn a_reply_in_pieces_hides_a_key_wherever_it_is_cut() {
        let (jwt, sendgrid) = (jwt!(), sendgrid!());
        let whole = format!("The file sets OPENAI_API_KEY={KEY} and DEBUG=1. The token {KEY}, ghp_ABCDEFGHIJKLMNOPQRSTuvwxyz12 and AKIAIOSFODNN7EXAMPLE are keys; sk-learn is not. \
            Bearer {jwt} and {sendgrid} too.");
        let wanted = hide_keys(&whole).into_owned();
        assert!(wanted.contains("sk-proj-abcd…(hidden)") && !wanted.contains("EFGH1234"), "{wanted}");
        assert!(!wanted.contains("eyJzdWIi") && !wanted.contains("SflKxw") && !wanted.contains("ABCDEFGHIJ"), "a key of parts, all of it: {wanted}");
        let characters: Vec<usize> = whole.char_indices().map(|(at, _)| at).chain([whole.len()]).collect();
        // (cut in two at every place)
        for &at in &characters {
            let mut mask = StreamMask::default();
            let mut shown = mask.push(&whole[..at]);
            shown.push_str(&mask.push(&whole[at..]));
            shown.push_str(&mask.finish());
            assert_eq!(shown, wanted, "cut at {at}");
        }
        // (cut in three at every pair of places a few apart, and one character at a time)
        for &first in characters.iter().step_by(7) {
            for &second in characters.iter().filter(|second| **second >= first).step_by(5) {
                let mut mask = StreamMask::default();
                let mut shown = String::new();
                for piece in [&whole[..first], &whole[first..second], &whole[second..]] {
                    shown.push_str(&mask.push(piece));
                }
                shown.push_str(&mask.finish());
                assert_eq!(shown, wanted, "cut at {first} and {second}");
            }
        }
        let mut mask = StreamMask::default();
        let mut shown = String::new();
        for (at, c) in whole.char_indices() {
            shown.push_str(&mask.push(&whole[at..at + c.len_utf8()]));
        }
        shown.push_str(&mask.finish());
        assert_eq!(shown, wanted);
    }

    /// A word that is no key because of what stands before its prefix (`task-...`) is none when it comes in pieces cut between them.
    #[test]
    fn a_word_that_only_ends_like_a_key_is_none_in_pieces_either() {
        for word in ["task-0123456789abcdefghijklmn", "pre_ghp_ABCDEFGHIJKLMNOPQRSTuvwxyz12", "text and xxoxb-1234567890abc"] {
            for at in 0..=word.len() {
                let mut mask = StreamMask::default();
                let mut shown = mask.push(&word[..at]);
                shown.push_str(&mask.push(&word[at..]));
                shown.push_str(&mask.finish());
                assert_eq!(shown, word, "{word} cut at {at}");
            }
        }
    }

    /// What comes before a word that may be a key is shown at once, a word that is not one is not waited for, and the one that may be is held till the next piece says.
    #[test]
    fn only_a_word_that_may_be_a_key_is_waited_for() {
        let mut mask = StreamMask::default();
        assert_eq!(mask.push("the key is sk-pro"), "the key is ");
        assert_eq!(mask.push("j-abcdEFGH1234ijklMNOP5678 and"), "sk-proj-abcd…(hidden) and");
        // (a word that is nothing like a key goes out as it comes, even cut in the middle)
        assert_eq!(mask.push(" hello wor"), " hello wor");
        assert_eq!(mask.push("ld"), "ld");
        // (so does one that began like a key and turned out not to be)
        assert_eq!(mask.push(" sk"), " ");
        assert_eq!(mask.push("y blue"), "sky blue");
        // (and at the end of the reply what is held is let go)
        assert_eq!(mask.push(" ghp_"), " ");
        assert_eq!(mask.finish(), "ghp_");
        assert_eq!(mask.finish(), "");
        // (a very long word that begins like a key is no longer waited for)
        let long = format!("sk-{}", "a".repeat(500));
        assert_eq!(StreamMask::default().push(&long), long);
    }

    /// Words that only look like a prefix are left: no digit after `sk-`, a key too short, a prefix inside a word.
    #[test]
    fn what_is_no_key_is_left_as_it_is() {
        for text in ["pip install sk-learn-extra-tools-for-everything", "ask-me sk-short1", "task-0123456789abcdefghijklmn", "AKIAIOSFODNN7EXAMPLEX", "plain text"] {
            assert!(matches!(hide_keys(text), Cow::Borrowed(_)), "{text}");
        }
    }
}
