//! The Sensitive Data Scanner's standard patterns (`docs/observe.md` §8.9), as Datadog's library
//! names them: credit cards, bank accounts, personal data, network addresses and credentials. Each
//! a regular expression (Rust's `regex`: linear time, no look-around), what checks a match beyond
//! its shape (a card's Luhn digit, an IBAN's mod-97 checksum, an SSN's assigned ranges), keywords
//! one of which a match too loose alone must have near it (a 40-character AWS secret), and the
//! tags a match puts on its record (`sensitive_data:<id>`, `sensitive_data_category:<category>`).
//! A group named `v` in a pattern is what of a match is replaced (a URL's password, not its host).
//!
//! They are written with `\b` and `\d` and compiled ASCII's ([`ascii`]): a word's edge and a digit
//! as logs write them (a card's digits are ASCII's), and what lets `regex` run its lazy DFA on any
//! text (a Unicode word boundary sends it to a slower engine on the first byte past ASCII). A run
//! that may begin at any word has no upper bound (an email's local part, an IBAN's body, a URL's
//! scheme): counted, each count a state of every word's, they made the set of a group's patterns
//! thrash its DFA's cache (§12.28).

/// What checks a match beyond its shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validator {
    None,
    /// Its digits pass the Luhn check (ISO/IEC 7812-1), 12 to 19 of them.
    Luhn,
    /// A US Social Security number as assigned: its area neither 000, 666 nor 900–999, its group
    /// not 00, its serial not 0000 (SSA's rules, also since randomization in 2011).
    Ssn,
    /// An IBAN (ISO 13616): its country's length, and mod 97 of it, moved and lettered, 1.
    Iban,
    /// An IPv6 address as `std::net::Ipv6Addr` reads one.
    Ipv6,
}

impl Validator {
    pub fn ok(self, s: &str) -> bool {
        match self {
            Self::None => true,
            Self::Luhn => luhn(s),
            Self::Ssn => ssn(s),
            Self::Iban => iban(s),
            Self::Ipv6 => s.parse::<std::net::Ipv6Addr>().is_ok(),
        }
    }
}

/// A standard pattern.
#[derive(Debug, Clone, Copy)]
pub struct Standard {
    pub id: &'static str,
    pub name: &'static str,
    pub category: &'static str,
    pub description: &'static str,
    pub pattern: &'static str,
    pub validator: Validator,
    /// Keywords one of which, near a match, makes it count: always, when `keywords_required` (a
    /// pattern too loose alone), else when a rule asks for them.
    pub keywords: &'static [&'static str],
    pub keywords_required: bool,
    /// Datadog's priority: 1 (critical) to 5 (info).
    pub priority: u8,
}

impl Standard {
    /// The tags a match puts on its record.
    pub fn tags(&self) -> [String; 2] {
        [format!("sensitive_data:{}", self.id), format!("sensitive_data_category:{}", self.category)]
    }
}

const fn std(id: &'static str, name: &'static str, category: &'static str, description: &'static str, pattern: &'static str, validator: Validator, priority: u8) -> Standard {
    Standard { id, name, category, description, pattern, validator, keywords: &[], keywords_required: false, priority }
}

const fn keyed(mut s: Standard, keywords: &'static [&'static str], required: bool) -> Standard {
    s.keywords = keywords;
    s.keywords_required = required;
    s
}

/// The library, in the order a page lists it.
pub const STANDARD: &[Standard] = &[
    std(
        "visa_card",
        "Visa card",
        "credit_card",
        "A Visa card number (13, 16 or 19 digits, spaces or dashes between fours), its Luhn digit checked.",
        r"\b4\d{3}(?:(?:[ -]?\d{4}){3}(?:[ -]?\d{3})?|\d{9})\b",
        Validator::Luhn,
        1,
    ),
    std(
        "mastercard",
        "Mastercard",
        "credit_card",
        "A Mastercard number (51–55 and 2221–2720), its Luhn digit checked.",
        r"\b(?:5[1-5]\d{2}|222[1-9]|22[3-9]\d|2[3-6]\d{2}|27[01]\d|2720)(?:[ -]?\d{4}){3}\b",
        Validator::Luhn,
        1,
    ),
    std("amex_card", "American Express card", "credit_card", "An American Express number (34, 37; 15 digits), its Luhn digit checked.", r"\b3[47]\d{2}[ -]?\d{6}[ -]?\d{5}\b", Validator::Luhn, 1),
    std("discover_card", "Discover card", "credit_card", "A Discover number (6011, 644–649, 65), its Luhn digit checked.", r"\b6(?:011|4[4-9]\d|5\d{2})(?:[ -]?\d{4}){3}\b", Validator::Luhn, 1),
    std("jcb_card", "JCB card", "credit_card", "A JCB number (3528–3589), its Luhn digit checked.", r"\b35(?:2[89]|[3-8]\d)(?:[ -]?\d{4}){3}\b", Validator::Luhn, 1),
    std(
        "diners_card",
        "Diners Club card",
        "credit_card",
        "A Diners Club number (300–305, 36, 38; 14 digits), its Luhn digit checked.",
        r"\b3(?:0[0-5]|[68]\d)\d[ -]?\d{6}[ -]?\d{4}\b",
        Validator::Luhn,
        1,
    ),
    std("iban", "IBAN", "bank_account", "An international bank account number (ISO 13616), its country's length and checksum checked.", r"\b[A-Z]{2}\d{2}(?: ?[A-Z0-9]){11,}\b", Validator::Iban, 1),
    keyed(
        std("us_ssn", "US Social Security number", "pii", "A US Social Security number written with its dashes, in the ranges assigned.", r"\b\d{3}-\d{2}-\d{4}\b", Validator::Ssn, 1),
        &["ssn", "social security", "social_security", "socialsecurity"],
        false,
    ),
    std("email_address", "Email address", "pii", "An email address.", r"\b[A-Za-z0-9][A-Za-z0-9._%+-]*@(?:[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?\.)+[A-Za-z]{2,}\b", Validator::None, 2),
    std("international_phone_number", "International phone number", "pii", "A phone number in E.164's form (a plus, then 8 to 15 digits).", r"\+[1-9]\d{7,14}\b", Validator::None, 3),
    keyed(
        std(
            "us_phone_number",
            "US phone number",
            "pii",
            "A North American phone number ((555) 123-4567, 555.123.4567); near a keyword when asked.",
            r"(?:\(\d{3}\) ?|\b\d{3}[ .-])\d{3}[ .-]\d{4}\b",
            Validator::None,
            3,
        ),
        &["phone", "tel", "mobile", "cell", "fax"],
        false,
    ),
    std("ipv4_address", "IPv4 address", "network", "An IPv4 address in its dotted form.", r"\b(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(?:\.(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}\b", Validator::None, 4),
    std(
        "ipv6_address",
        "IPv6 address",
        "network",
        "An IPv6 address, whole or with its zeros compressed.",
        r"\b(?:[0-9A-Fa-f]{1,4}:){7}[0-9A-Fa-f]{1,4}\b|\b(?:[0-9A-Fa-f]{1,4}:){1,6}(?::[0-9A-Fa-f]{1,4}){1,6}\b|\b(?:[0-9A-Fa-f]{1,4}:){1,7}:",
        Validator::Ipv6,
        4,
    ),
    std("mac_address", "MAC address", "network", "A MAC address (six hex pairs, colons or dashes between).", r"\b[0-9A-Fa-f]{2}(?:[:-][0-9A-Fa-f]{2}){5}\b", Validator::None, 5),
    std(
        "aws_access_key_id",
        "AWS access key ID",
        "credentials",
        "An AWS access key's ID (AKIA, ASIA and kin, then 16 of base32).",
        r"\b(?:AKIA|ASIA|ABIA|ACCA|AGPA|AIDA|AIPA|ANPA|ANVA|APKA|AROA)[A-Z2-7]{16}\b",
        Validator::None,
        1,
    ),
    keyed(
        std("aws_secret_access_key", "AWS secret access key", "credentials", "An AWS secret access key (40 of base64), near a keyword naming one.", r"\b[A-Za-z0-9/+]{40}\b", Validator::None, 1),
        &["aws_secret_access_key", "aws_secret", "secret_access_key", "secretaccesskey", "aws secret"],
        true,
    ),
    std(
        "github_token",
        "GitHub token",
        "credentials",
        "A GitHub token: personal, OAuth, app or refresh (ghp_, gho_, ghu_, ghs_, ghr_), or fine-grained (github_pat_).",
        r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{82})\b",
        Validator::None,
        1,
    ),
    std("gitlab_token", "GitLab token", "credentials", "A GitLab personal access token (glpat-).", r"\bglpat-[A-Za-z0-9_-]{20,}\b", Validator::None, 1),
    std("slack_token", "Slack token", "credentials", "A Slack token (xoxb-, xoxp-, xoxa-, xoxo-, xoxs-, xoxr-).", r"\bxox[abposr]-[A-Za-z0-9-]{10,}\b", Validator::None, 1),
    std(
        "slack_webhook_url",
        "Slack webhook URL",
        "credentials",
        "A Slack incoming webhook's URL.",
        r"https://hooks\.slack\.com/services/T[A-Z0-9]{8,12}/B[A-Z0-9]{8,12}/[A-Za-z0-9]{24}",
        Validator::None,
        1,
    ),
    std("stripe_secret_key", "Stripe secret key", "credentials", "A Stripe secret or restricted key (sk_, rk_; live or test).", r"\b[sr]k_(?:live|test)_[A-Za-z0-9]{24,}\b", Validator::None, 1),
    std("google_api_key", "Google API key", "credentials", "A Google API key (AIza, then 35 more).", r"\bAIza[0-9A-Za-z_-]{35}\b", Validator::None, 1),
    std("npm_token", "npm token", "credentials", "An npm access token (npm_).", r"\bnpm_[A-Za-z0-9]{36}\b", Validator::None, 1),
    std("sendgrid_api_key", "SendGrid API key", "credentials", "A SendGrid API key (SG., then its two parts).", r"\bSG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}\b", Validator::None, 1),
    std(
        "openai_api_key",
        "OpenAI API key",
        "credentials",
        "An OpenAI API key (sk-, its T3BlbkFJ marker within).",
        r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}T3BlbkFJ[A-Za-z0-9_-]{20,}",
        Validator::None,
        1,
    ),
    std("anthropic_api_key", "Anthropic API key", "credentials", "An Anthropic API key (sk-ant-).", r"\bsk-ant-(?:api|admin)\d{2}-[A-Za-z0-9_-]{80,}", Validator::None, 1),
    std(
        "json_web_token",
        "JSON Web Token",
        "credentials",
        "A JSON Web Token: base64url header and payload (each a JSON object, eyJ), and its signature.",
        r"\beyJ[A-Za-z0-9_-]{5,}\.eyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{10,}",
        Validator::None,
        2,
    ),
    std(
        "private_key",
        "Private key",
        "credentials",
        "A private key in PEM's armor (RSA, EC, DSA, OpenSSH, PKCS #8, PGP), whole.",
        r"-----BEGIN[A-Z ]{0,30} PRIVATE KEY(?: BLOCK)?-----[\s\S]*?-----END[A-Z ]{0,30} PRIVATE KEY(?: BLOCK)?-----",
        Validator::None,
        1,
    ),
    std(
        "password_in_url",
        "Password in a URL",
        "credentials",
        "A URL's password (scheme://user:password@host): the password alone replaced.",
        r"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s:@/]+:(?P<v>[^\s@/]+)@",
        Validator::None,
        1,
    ),
    std("bearer_token", "Bearer token", "credentials", "A bearer token (an Authorization header's): the token alone replaced.", r"(?i)\bbearer\s+(?P<v>[A-Za-z0-9._~+/-]{16,}=*)", Validator::None, 1),
    std(
        "secret_assignment",
        "Secret assigned",
        "credentials",
        "A password, secret, API key or token given its value (password=…, \"api_key\": \"…\"): the value alone replaced.",
        r#"(?i)\b(?:password|passwd|pwd|secret|client[_-]?secret|api[_-]?key|access[_-]?token|auth[_-]?token|refresh[_-]?token)["']?\s*[:=]\s*["']?(?P<v>[^\s"',;&]{4,})"#,
        Validator::None,
        2,
    ),
    std(
        "azure_storage_key",
        "Azure storage account key",
        "credentials",
        "An Azure Storage account key in a connection string (AccountKey=…).",
        r"\bAccountKey=(?P<v>[A-Za-z0-9+/]{86}==)",
        Validator::None,
        1,
    ),
    keyed(
        std("datadog_api_key", "Datadog API key", "credentials", "A Datadog API key (32 hex), near a keyword naming one.", r"\b[a-f0-9]{32}\b", Validator::None, 1),
        &["dd_api_key", "datadog_api_key", "dd-api-key", "datadog api key", "api_key"],
        true,
    ),
];

/// A standard pattern by its id.
pub fn find(id: &str) -> Option<&'static Standard> {
    STANDARD.iter().find(|s| s.id == id)
}

/// A library pattern as it is compiled: its `\b` ASCII's word boundary, its `\d` ASCII's digits.
pub fn ascii(p: &str) -> String {
    p.replace(r"\b", r"(?-u:\b)").replace(r"\d", "[0-9]")
}

fn digits(s: &str) -> impl Iterator<Item = u32> + '_ {
    s.chars().filter_map(|c| c.to_digit(10))
}

/// The Luhn check of `s`'s digits (separators passed over), 12 to 19 of them: every second digit
/// from the right doubled (its digits summed), the whole a multiple of 10.
pub fn luhn(s: &str) -> bool {
    let n = digits(s).count();
    if !(12..=19).contains(&n) {
        return false;
    }
    // From the left, digit `i` is `n - 1 - i` from the right: doubled when that is odd.
    let sum: u32 = digits(s)
        .enumerate()
        .map(|(i, d)| {
            if (n - i).is_multiple_of(2) {
                let x = d * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// An SSN (`AAA-GG-SSSS`) in the ranges the SSA assigns.
pub fn ssn(s: &str) -> bool {
    let mut parts = s.split('-');
    let (Some(a), Some(g), Some(n), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return false };
    let num = |p: &str, len: usize| (p.len() == len && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse::<u32>().ok()).flatten();
    let (Some(area), Some(group), Some(serial)) = (num(a, 3), num(g, 2), num(n, 4)) else { return false };
    area != 0 && area != 666 && area < 900 && group != 0 && serial != 0
}

/// IBAN lengths by country (SWIFT's registry, release 98).
const IBAN_LENGTHS: &[(&str, usize)] = &[
    ("AD", 24),
    ("AE", 23),
    ("AL", 28),
    ("AT", 20),
    ("AZ", 28),
    ("BA", 20),
    ("BE", 16),
    ("BG", 22),
    ("BH", 22),
    ("BI", 27),
    ("BR", 29),
    ("BY", 28),
    ("CH", 21),
    ("CR", 22),
    ("CY", 28),
    ("CZ", 24),
    ("DE", 22),
    ("DJ", 27),
    ("DK", 18),
    ("DO", 28),
    ("EE", 20),
    ("EG", 29),
    ("ES", 24),
    ("FI", 18),
    ("FK", 18),
    ("FO", 18),
    ("FR", 27),
    ("GB", 22),
    ("GE", 22),
    ("GI", 23),
    ("GL", 18),
    ("GR", 27),
    ("GT", 28),
    ("HR", 21),
    ("HU", 28),
    ("IE", 22),
    ("IL", 23),
    ("IQ", 23),
    ("IS", 26),
    ("IT", 27),
    ("JO", 30),
    ("KW", 30),
    ("KZ", 20),
    ("LB", 28),
    ("LC", 32),
    ("LI", 21),
    ("LT", 20),
    ("LU", 20),
    ("LV", 21),
    ("LY", 25),
    ("MC", 27),
    ("MD", 24),
    ("ME", 22),
    ("MK", 19),
    ("MN", 20),
    ("MR", 27),
    ("MT", 31),
    ("MU", 30),
    ("NI", 28),
    ("NL", 18),
    ("NO", 15),
    ("OM", 23),
    ("PK", 24),
    ("PL", 28),
    ("PS", 29),
    ("PT", 25),
    ("QA", 29),
    ("RO", 24),
    ("RS", 22),
    ("RU", 33),
    ("SA", 24),
    ("SC", 31),
    ("SD", 18),
    ("SE", 24),
    ("SI", 19),
    ("SK", 24),
    ("SM", 27),
    ("SO", 23),
    ("ST", 25),
    ("SV", 28),
    ("TL", 23),
    ("TN", 24),
    ("TR", 26),
    ("UA", 29),
    ("VA", 22),
    ("VG", 24),
    ("XK", 20),
    ("YE", 30),
];

/// An IBAN (spaces passed over): its country's length, and its four first characters moved to its
/// end, each letter its number (A = 10 … Z = 35), the whole mod 97 is 1.
pub fn iban(s: &str) -> bool {
    let chars: Vec<u8> = s.bytes().filter(|b| *b != b' ').collect();
    let Some(country) = chars.get(..2).and_then(|c| std::str::from_utf8(c).ok()) else { return false };
    let Some(&(_, len)) = IBAN_LENGTHS.iter().find(|(c, _)| *c == country) else { return false };
    if chars.len() != len {
        return false;
    }
    let mut rem = 0u32;
    for &c in chars.iter().skip(4).chain(chars.iter().take(4)) {
        let v = match c {
            b'0'..=b'9' => u32::from(c - b'0'),
            b'A'..=b'Z' => u32::from(c - b'A') + 10,
            _ => return false,
        };
        rem = if v >= 10 { (rem * 100 + v) % 97 } else { (rem * 10 + v) % 97 };
    }
    rem == 1
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Every standard pattern compiles within a rule's bound, and all of them as one group's set
    /// within a set's; ids unique.
    #[test]
    fn every_standard_pattern_compiles() {
        let mut ids: Vec<&str> = STANDARD.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), STANDARD.len(), "ids unique");
        for s in STANDARD {
            let built = super::super::build(&ascii(s.pattern));
            assert!(built.is_ok(), "{}: {:?}", s.id, built.err());
            assert!((1..=5).contains(&s.priority), "{}", s.id);
        }
        let all: Vec<String> = STANDARD.iter().map(|s| ascii(s.pattern)).collect();
        super::super::build_set(&all).unwrap();
    }

    /// The checks: cards' Luhn digits (Visa's and Amex's test numbers pass, one digit off fails),
    /// SSNs' ranges, IBANs' checksums (ECBS's examples).
    #[test]
    fn checks_hold_what_their_standards_say() {
        for good in ["4111 1111 1111 1111", "4111-1111-1111-1111", "378282246310005", "5555555555554444", "6011111111111117", "3530111333300000", "30569309025904", "4222222222222"] {
            assert!(luhn(good), "{good}");
        }
        for bad in ["4111 1111 1111 1112", "1234", "41111111111111111111"] {
            assert!(!luhn(bad), "{bad}");
        }
        for good in ["123-45-6789", "078-05-1120", "899-01-0001"] {
            assert!(ssn(good), "{good}");
        }
        for bad in ["000-12-3456", "666-12-3456", "912-12-3456", "123-00-4567", "123-45-0000", "12-345-6789"] {
            assert!(!ssn(bad), "{bad}");
        }
        for good in ["GB82 WEST 1234 5698 7654 32", "DE89370400440532013000", "FR1420041010050500013M02606", "NO9386011117947"] {
            assert!(iban(good), "{good}");
        }
        for bad in ["GB82 WEST 1234 5698 7654 33", "DE8937040044053201300", "ZZ89370400440532013000"] {
            assert!(!iban(bad), "{bad}");
        }
        assert!(Validator::Ipv6.ok("2001:db8::ff00:42:8329") && !Validator::Ipv6.ok("2001:db8:::1"));
    }
}
