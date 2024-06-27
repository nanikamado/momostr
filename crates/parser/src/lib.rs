use nom::branch::alt;
use nom::bytes::complete::{is_not, tag, take_while1};
use nom::combinator::opt;
use nom::sequence::{delimited, preceded, tuple};
use nom::{IResult, Parser};

fn word(i: &str) -> IResult<&str, &str, ()> {
    take_while1(|a: char| a.is_ascii_alphanumeric() || a == '.' || a == '-')(i)
}

fn mention_inner(i: &str) -> IResult<&str, (&str, Option<&str>), ()> {
    tuple((
        tag::<&str, &str, ()>("@"),
        word,
        opt(preceded(tag("@"), word)),
    ))
    .map(|(_, username, domain)| (username, domain))
    .parse(i)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Mention<'a> {
    pub username: &'a str,
    pub domain: Option<&'a str>,
    pub url: Option<&'a str>,
}

fn single_mention(i: &str) -> IResult<&str, Mention, ()> {
    alt((
        tuple((
            delimited(tag("["), mention_inner, tag("]")),
            opt(delimited(tag("("), is_not(")\r\n"), tag(")"))),
        ))
        .map(|((username, domain), url)| Mention {
            username,
            domain,
            url,
        }),
        mention_inner.map(|(username, domain)| Mention {
            username,
            domain,
            url: None,
        }),
    ))
    .parse(i)
}

pub fn mention(input: &str) -> Result<(&str, Mention, &str), &str> {
    let mut i = 0;
    let mut s = input;
    loop {
        if let Ok((r, m)) = single_mention(s) {
            return Ok((&input[..i], m, r));
        }
        let mut cs = s.chars();
        let Some(c) = cs.next() else {
            return Err(&input[..i]);
        };
        i += c.len_utf8();
        s = cs.as_str();
    }
}

#[test]
fn test_mention() {
    assert_eq!(mention("").unwrap_err(), "");
    assert_eq!(mention("aaaaaaaaa").unwrap_err(), "aaaaaaaaa");
    assert_eq!(mention("@@@@@").unwrap_err(), "@@@@@");
    assert_eq!(
        mention("@0-0a..-").unwrap(),
        (
            "",
            Mention {
                username: "0-0a..-",
                domain: None,
                url: None
            },
            ""
        )
    );
    assert_eq!(
        mention("[[[[[@0-0a..-@aaa.aaa   ").unwrap(),
        (
            "[[[[[",
            Mention {
                username: "0-0a..-",
                domain: Some("aaa.aaa"),
                url: None
            },
            "   "
        )
    );
    assert_eq!(
        mention("[[@0-0a..-@aaa.aaa](aaa )]()").unwrap(),
        (
            "[",
            Mention {
                username: "0-0a..-",
                domain: Some("aaa.aaa"),
                url: Some("aaa ")
            },
            "]()"
        )
    );
}
