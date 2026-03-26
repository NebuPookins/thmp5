use anyhow::{bail, Context};
use pest::iterators::Pair;
use pest::Parser;
use pest_derive::Parser;
use std::fmt::Write;

// ── Pest parser ───────────────────────────────────────────────────────────────

#[derive(Parser)]
#[grammar = "query/grammar.pest"]
struct QueryParser;

// ── AST ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Pred(Predicate),
}

#[derive(Debug, Clone)]
pub enum Predicate {
    RatingNull,
    Rating(CmpOp, i64),
    AlbumRating(CmpOp, i64),
    LastPlayed(LastPlayedDir, i64, TimeUnit),
    PlayCount(CmpOp, i64),
    InPlaylist(String),
    NotInPlaylist(String),
    HasTag(String),
    Title(StrOp, String),
    Artist(StrOp, String),
    Genre(StrOp, String),
    Year(CmpOp, i64),
}

#[derive(Debug, Clone, Copy)]
pub enum CmpOp {
    Gte,
    Lte,
    Gt,
    Lt,
    Eq,
    Ne,
}

#[derive(Debug, Clone, Copy)]
pub enum StrOp {
    Contains,
    Is,
}

#[derive(Debug, Clone, Copy)]
pub enum LastPlayedDir {
    InLast,
    NotInLast,
}

#[derive(Debug, Clone, Copy)]
pub enum TimeUnit {
    Days,
    Weeks,
    Months,
    Years,
}

#[derive(Debug, Clone, Copy)]
pub enum LimitUnit {
    Tracks,
    Minutes,
    Hours,
}

#[derive(Debug, Clone)]
pub struct Limit {
    pub value: i64,
    pub unit: LimitUnit,
}

#[derive(Debug, Clone)]
pub struct ParsedQuery {
    pub expr: Expr,
    pub limit: Option<Limit>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Parse a smart-playlist query string into an AST.
pub fn parse(input: &str) -> anyhow::Result<ParsedQuery> {
    let mut pairs = QueryParser::parse(Rule::query, input)
        .context("Query syntax error")?;
    let query_pair = pairs.next().context("empty parse")?;

    let mut expr: Option<Expr> = None;
    let mut limit: Option<Limit> = None;

    for inner in query_pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => {
                expr = Some(parse_expr(inner)?);
            }
            Rule::limit => {
                limit = Some(parse_limit(inner)?);
            }
            Rule::EOI => {}
            _ => {}
        }
    }

    Ok(ParsedQuery {
        expr: expr.context("missing expression")?,
        limit,
    })
}

/// Compile a parsed query to a SQL WHERE clause fragment suitable for use in
/// `SELECT … FROM smart_playlist_view WHERE {where_sql}`.
/// Returns `(where_sql, limit)`.
pub fn compile(query: &ParsedQuery) -> (String, Option<Limit>) {
    let mut where_sql = String::new();
    compile_expr(&query.expr, &mut where_sql);
    (where_sql, query.limit.clone())
}

// ── Parse helpers ─────────────────────────────────────────────────────────────

fn parse_expr(pair: Pair<Rule>) -> anyhow::Result<Expr> {
    // expr = { or_expr }
    let inner = pair.into_inner().next().context("expr: missing or_expr")?;
    parse_or_expr(inner)
}

fn parse_or_expr(pair: Pair<Rule>) -> anyhow::Result<Expr> {
    // or_expr = { and_expr ~ ("OR" ~ and_expr)* }
    let mut children = pair.into_inner().map(parse_and_expr);
    let first = children.next().context("or_expr: empty")?? ;
    children.try_fold(first, |acc, next| Ok(Expr::Or(Box::new(acc), Box::new(next?))))
}

fn parse_and_expr(pair: Pair<Rule>) -> anyhow::Result<Expr> {
    // and_expr = { factor ~ ("AND" ~ factor)* }
    let mut children = pair.into_inner().map(parse_factor);
    let first = children.next().context("and_expr: empty")?? ;
    children.try_fold(first, |acc, next| Ok(Expr::And(Box::new(acc), Box::new(next?))))
}

fn parse_factor(pair: Pair<Rule>) -> anyhow::Result<Expr> {
    let inner = pair.into_inner().next().context("factor: empty")?;
    match inner.as_rule() {
        Rule::paren_expr => {
            let expr_pair = inner.into_inner().next().context("paren_expr: missing expr")?;
            parse_expr(expr_pair)
        }
        Rule::not_expr => {
            let factor_pair = inner.into_inner().next().context("not_expr: missing factor")?;
            Ok(Expr::Not(Box::new(parse_factor(factor_pair)?)))
        }
        Rule::predicate => parse_predicate(inner),
        r => bail!("factor: unexpected rule {:?}", r),
    }
}

fn parse_predicate(pair: Pair<Rule>) -> anyhow::Result<Expr> {
    let inner = pair.into_inner().next().context("predicate: empty")?;
    let pred = match inner.as_rule() {
        Rule::rating_pred => parse_rating_pred(inner)?,
        Rule::album_rating_pred => {
            let mut it = inner.into_inner();
            let op = parse_cmp_op(it.next().context("album_rating: missing op")?)?;
            let n = parse_integer(it.next().context("album_rating: missing n")?)?;
            Predicate::AlbumRating(op, n)
        }
        Rule::last_played_pred => parse_last_played_pred(inner)?,
        Rule::play_count_pred => {
            let mut it = inner.into_inner();
            let op = parse_cmp_op(it.next().context("play_count: missing op")?)?;
            let n = parse_integer(it.next().context("play_count: missing n")?)?;
            Predicate::PlayCount(op, n)
        }
        Rule::in_playlist_pred => {
            let name = extract_string(inner.into_inner().next().context("InPlaylist: missing name")?)?;
            Predicate::InPlaylist(name)
        }
        Rule::not_in_playlist_pred => {
            let name = extract_string(inner.into_inner().next().context("NotInPlaylist: missing name")?)?;
            Predicate::NotInPlaylist(name)
        }
        Rule::has_tag_pred => {
            let tag = extract_string(inner.into_inner().next().context("HasTag: missing tag")?)?;
            Predicate::HasTag(tag)
        }
        Rule::title_pred => parse_str_pred(inner, |op, s| Predicate::Title(op, s))?,
        Rule::artist_pred => parse_str_pred(inner, |op, s| Predicate::Artist(op, s))?,
        Rule::genre_pred => parse_str_pred(inner, |op, s| Predicate::Genre(op, s))?,
        Rule::year_pred => {
            let mut it = inner.into_inner();
            let op = parse_cmp_op(it.next().context("Year: missing op")?)?;
            let n = parse_integer(it.next().context("Year: missing n")?)?;
            Predicate::Year(op, n)
        }
        r => bail!("predicate: unexpected rule {:?}", r),
    };
    Ok(Expr::Pred(pred))
}

fn parse_rating_pred(pair: Pair<Rule>) -> anyhow::Result<Predicate> {
    let inner = pair.into_inner().next().context("rating_pred: empty")?;
    match inner.as_rule() {
        Rule::rating_null => Ok(Predicate::RatingNull),
        Rule::cmp_pred => {
            let mut it = inner.into_inner();
            let op = parse_cmp_op(it.next().context("rating cmp: missing op")?)?;
            let n = parse_integer(it.next().context("rating cmp: missing n")?)?;
            Ok(Predicate::Rating(op, n))
        }
        r => bail!("rating_pred: unexpected {:?}", r),
    }
}

fn parse_last_played_pred(pair: Pair<Rule>) -> anyhow::Result<Predicate> {
    let mut it = pair.into_inner();
    let dir_pair = it.next().context("LastPlayed: missing dir")?;
    let dir = match dir_pair.as_str() {
        "InLast" => LastPlayedDir::InLast,
        "NotInLast" => LastPlayedDir::NotInLast,
        s => bail!("LastPlayed: unknown dir {:?}", s),
    };
    let n = parse_integer(it.next().context("LastPlayed: missing n")?)?;
    let unit_pair = it.next().context("LastPlayed: missing unit")?;
    let unit = parse_time_unit(unit_pair)?;
    Ok(Predicate::LastPlayed(dir, n, unit))
}

fn parse_str_pred<F>(pair: Pair<Rule>, mk: F) -> anyhow::Result<Predicate>
where
    F: Fn(StrOp, String) -> Predicate,
{
    let mut it = pair.into_inner();
    let op_pair = it.next().context("str_pred: missing op")?;
    let op = match op_pair.as_str() {
        "contains" => StrOp::Contains,
        "is" => StrOp::Is,
        s => bail!("str_pred: unknown op {:?}", s),
    };
    let s = extract_string(it.next().context("str_pred: missing string")?)?;
    Ok(mk(op, s))
}

fn parse_limit(pair: Pair<Rule>) -> anyhow::Result<Limit> {
    let mut it = pair.into_inner();
    let n = parse_integer(it.next().context("limit: missing n")?)?;
    let unit_pair = it.next().context("limit: missing unit")?;
    let unit = match unit_pair.as_str() {
        "tracks" => LimitUnit::Tracks,
        "minutes" => LimitUnit::Minutes,
        "hours" => LimitUnit::Hours,
        s => bail!("limit: unknown unit {:?}", s),
    };
    Ok(Limit { value: n, unit })
}

fn parse_cmp_op(pair: Pair<Rule>) -> anyhow::Result<CmpOp> {
    match pair.as_str() {
        ">=" => Ok(CmpOp::Gte),
        "<=" => Ok(CmpOp::Lte),
        ">"  => Ok(CmpOp::Gt),
        "<"  => Ok(CmpOp::Lt),
        "="  => Ok(CmpOp::Eq),
        "!=" => Ok(CmpOp::Ne),
        s    => bail!("cmp_op: unknown {:?}", s),
    }
}

fn parse_time_unit(pair: Pair<Rule>) -> anyhow::Result<TimeUnit> {
    match pair.as_str() {
        "Days"   => Ok(TimeUnit::Days),
        "Weeks"  => Ok(TimeUnit::Weeks),
        "Months" => Ok(TimeUnit::Months),
        "Years"  => Ok(TimeUnit::Years),
        s        => bail!("time_unit: unknown {:?}", s),
    }
}

fn parse_integer(pair: Pair<Rule>) -> anyhow::Result<i64> {
    pair.as_str().parse::<i64>().context("integer parse error")
}

/// Extract the string value from a `string` rule pair (quoted or bare ident).
fn extract_string(pair: Pair<Rule>) -> anyhow::Result<String> {
    // string = { quoted_string | ident }
    let inner = pair.into_inner().next().context("string: empty")?;
    match inner.as_rule() {
        Rule::quoted_string => {
            // quoted_string = { "\"" ~ quoted_inner ~ "\"" }
            let content = inner
                .into_inner()
                .next()
                .context("quoted_string: missing inner")?;
            Ok(content.as_str().to_owned())
        }
        Rule::ident => Ok(inner.as_str().to_owned()),
        r => bail!("string: unexpected rule {:?}", r),
    }
}

// ── SQL compiler ──────────────────────────────────────────────────────────────

/// Escape a string value for safe embedding in a SQL literal.
/// Doubles any single-quote characters.
fn sql_esc(s: &str) -> String {
    s.replace('\'', "''")
}

fn cmp_op_sql(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Gte => ">=",
        CmpOp::Lte => "<=",
        CmpOp::Gt  => ">",
        CmpOp::Lt  => "<",
        CmpOp::Eq  => "=",
        CmpOp::Ne  => "!=",
    }
}

fn compile_expr(expr: &Expr, out: &mut String) {
    match expr {
        Expr::And(a, b) => {
            write!(out, "(").unwrap();
            compile_expr(a, out);
            write!(out, " AND ").unwrap();
            compile_expr(b, out);
            write!(out, ")").unwrap();
        }
        Expr::Or(a, b) => {
            write!(out, "(").unwrap();
            compile_expr(a, out);
            write!(out, " OR ").unwrap();
            compile_expr(b, out);
            write!(out, ")").unwrap();
        }
        Expr::Not(e) => {
            write!(out, "NOT (").unwrap();
            compile_expr(e, out);
            write!(out, ")").unwrap();
        }
        Expr::Pred(pred) => compile_predicate(pred, out),
    }
}

fn compile_predicate(pred: &Predicate, out: &mut String) {
    match pred {
        Predicate::RatingNull => {
            write!(out, "rating IS NULL").unwrap();
        }
        Predicate::Rating(op, n) => {
            write!(out, "rating {} {}", cmp_op_sql(*op), n).unwrap();
        }
        Predicate::AlbumRating(op, n) => {
            write!(out, "album_rating {} {}", cmp_op_sql(*op), n).unwrap();
        }
        Predicate::LastPlayed(dir, n, unit) => {
            let sql_unit = match unit {
                TimeUnit::Days   => "days",
                TimeUnit::Weeks  => "days", // SQLite uses days
                TimeUnit::Months => "months",
                TimeUnit::Years  => "years",
            };
            // For Weeks, multiply n by 7
            let n_adjusted = match unit {
                TimeUnit::Weeks => n * 7,
                _ => *n,
            };
            match dir {
                LastPlayedDir::InLast => {
                    write!(
                        out,
                        "(last_played IS NOT NULL AND last_played >= datetime('now', '-{} {}'))",
                        n_adjusted, sql_unit
                    ).unwrap();
                }
                LastPlayedDir::NotInLast => {
                    write!(
                        out,
                        "(last_played IS NULL OR last_played < datetime('now', '-{} {}'))",
                        n_adjusted, sql_unit
                    ).unwrap();
                }
            }
        }
        Predicate::PlayCount(op, n) => {
            write!(out, "play_count {} {}", cmp_op_sql(*op), n).unwrap();
        }
        Predicate::InPlaylist(name) => {
            write!(
                out,
                "id IN (SELECT pt.recording_id FROM playlist_track pt \
                 JOIN playlist p ON p.id = pt.playlist_id WHERE p.name = '{}')",
                sql_esc(name)
            ).unwrap();
        }
        Predicate::NotInPlaylist(name) => {
            write!(
                out,
                "id NOT IN (SELECT pt.recording_id FROM playlist_track pt \
                 JOIN playlist p ON p.id = pt.playlist_id WHERE p.name = '{}')",
                sql_esc(name)
            ).unwrap();
        }
        Predicate::HasTag(tag) => {
            write!(
                out,
                "id IN (SELECT recording_id FROM recording_tag WHERE tag = '{}')",
                sql_esc(tag)
            ).unwrap();
        }
        Predicate::Title(op, s) => match op {
            StrOp::Contains => write!(
                out,
                "lower(title) LIKE '%{}%' ESCAPE '\\'",
                sql_like_esc(&s.to_lowercase())
            ).unwrap(),
            StrOp::Is => write!(out, "lower(title) = '{}'", sql_esc(&s.to_lowercase())).unwrap(),
        },
        Predicate::Genre(op, s) => match op {
            StrOp::Contains => write!(
                out,
                "(genre IS NOT NULL AND lower(genre) LIKE '%{}%' ESCAPE '\\')",
                sql_like_esc(&s.to_lowercase())
            ).unwrap(),
            StrOp::Is => write!(
                out,
                "lower(genre) = '{}'",
                sql_esc(&s.to_lowercase())
            ).unwrap(),
        },
        Predicate::Artist(op, s) => {
            // smart_playlist_view doesn't include artist name — use a subquery
            let pattern = match op {
                StrOp::Contains => format!("LIKE '%{}%' ESCAPE '\\'", sql_like_esc(&s.to_lowercase())),
                StrOp::Is => format!("= '{}'", sql_esc(&s.to_lowercase())),
            };
            write!(
                out,
                "id IN (SELECT ra.recording_id FROM recording_artist ra \
                 JOIN artist a ON a.id = ra.artist_id \
                 WHERE lower(COALESCE(ra.credited_as, a.name)) {})",
                pattern
            ).unwrap();
        }
        Predicate::Year(op, n) => {
            // smart_playlist_view doesn't include year — use a subquery
            write!(
                out,
                "id IN (SELECT DISTINCT t.recording_id FROM track t \
                 JOIN medium m ON m.id = t.medium_id \
                 JOIN release rel ON rel.id = m.release_id \
                 WHERE rel.release_date IS NOT NULL \
                 AND CAST(substr(rel.release_date, 1, 4) AS INTEGER) {} {})",
                cmp_op_sql(*op), n
            ).unwrap();
        }
    }
}

/// Escape special LIKE pattern characters %, _, \.
fn sql_like_esc(s: &str) -> String {
    s.replace('\\', "\\\\")
     .replace('%', "\\%")
     .replace('_', "\\_")
     .replace('\'', "''")
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_compile(q: &str) -> String {
        let parsed = parse(q).unwrap();
        compile(&parsed).0
    }

    #[test]
    fn test_rating_gte() {
        assert_eq!(parse_and_compile("Rating >= 4"), "rating >= 4");
    }

    #[test]
    fn test_rating_null() {
        assert_eq!(parse_and_compile("Rating is null"), "rating IS NULL");
    }

    #[test]
    fn test_album_rating() {
        assert_eq!(parse_and_compile("AlbumRating > 3"), "album_rating > 3");
    }

    #[test]
    fn test_play_count() {
        assert_eq!(parse_and_compile("PlayCount >= 10"), "play_count >= 10");
    }

    #[test]
    fn test_and_or() {
        let q = parse_and_compile("Rating >= 4 AND PlayCount >= 2");
        assert_eq!(q, "(rating >= 4 AND play_count >= 2)");
    }

    #[test]
    fn test_not() {
        let q = parse_and_compile("NOT Rating is null");
        assert_eq!(q, "NOT (rating IS NULL)");
    }

    #[test]
    fn test_has_tag() {
        let q = parse_and_compile("HasTag chill");
        assert_eq!(q, "id IN (SELECT recording_id FROM recording_tag WHERE tag = 'chill')");
    }

    #[test]
    fn test_title_contains() {
        let q = parse_and_compile(r#"Title contains "blue""#);
        assert_eq!(q, "lower(title) LIKE '%blue%' ESCAPE '\\'");
    }

    #[test]
    fn test_last_played_not_in_last() {
        let q = parse_and_compile("LastPlayed NotInLast 8 Months");
        assert_eq!(
            q,
            "(last_played IS NULL OR last_played < datetime('now', '-8 months'))"
        );
    }

    #[test]
    fn test_limit_parses() {
        let parsed = parse("Rating >= 4 LIMIT 60 minutes").unwrap();
        assert!(parsed.limit.is_some());
        let lim = parsed.limit.unwrap();
        assert_eq!(lim.value, 60);
        assert!(matches!(lim.unit, LimitUnit::Minutes));
    }

    #[test]
    fn test_parens() {
        let q = parse_and_compile("(Rating >= 4 OR Rating is null) AND PlayCount >= 1");
        assert_eq!(q, "((rating >= 4 OR rating IS NULL) AND play_count >= 1)");
    }

    #[test]
    fn test_sql_injection_escaped() {
        let q = parse_and_compile("HasTag \"evil'tag\"");
        assert!(q.contains("evil''tag"));
    }
}
