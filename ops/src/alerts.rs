// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The alert rules and paging routes, evaluated (`T-018-05`, `NFR-9`, `NFR-11`, `REQ-OP-6`,
//! `AC-C4.1`, `RISK-7`).
//!
//! The rules live in `ops/prometheus/rules/aledgent.yml` and the routes in
//! `ops/alertmanager/alertmanager.yml`, in the formats Prometheus and Alertmanager read. There is
//! no staging environment and no paging service in this repository's reach, so the tests evaluate
//! **those very files** here — the rule expressions against the exporter's rendered metrics, the
//! `for` durations over time, the routing tree to its receiver — and a test receiver stands for
//! the pager. The subset evaluated is the one the files use, and [`Rules::parse`] refuses any
//! other: every expression is `metric{label="value",...} op integer`, or such comparisons joined
//! by `and` (`T-018-13`), and every matcher of the routes is `label="value"`.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::Error;

/// The committed rules.
pub const RULES: &str = include_str!("../prometheus/rules/aledgent.yml");
/// The committed routes.
pub const ROUTES: &str = include_str!("../alertmanager/alertmanager.yml");

/// A comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
}

impl Op {
    fn holds(self, a: i128, b: i128) -> bool {
        match self {
            Op::Gt => a > b,
            Op::Ge => a >= b,
            Op::Lt => a < b,
            Op::Le => a <= b,
            Op::Eq => a == b,
            Op::Ne => a != b,
        }
    }
}

/// A rule's expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expr {
    /// The metric.
    pub metric: String,
    /// Label equalities it selects on.
    pub matchers: BTreeMap<String, String>,
    /// The comparison.
    pub op: Op,
    /// The line.
    pub threshold: i128,
    /// The comparison joined by `and`, if any (`T-018-13`): as Prometheus's `and`, a sample of
    /// this one is kept only if a sample with the same labels holds it too.
    pub and: Option<Box<Expr>>,
}

fn parse_matchers(body: &str) -> Result<BTreeMap<String, String>, Error> {
    let mut out = BTreeMap::new();
    for m in body.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        let (k, v) = m
            .split_once('=')
            .ok_or_else(|| Error::Config(format!("{m:?} is not label=\"value\"")))?;
        let v = v.trim();
        if k.ends_with('!') || v.starts_with('~') || !(v.starts_with('"') && v.ends_with('"')) {
            return Err(Error::Config(format!(
                "{m:?}: only label=\"value\" matchers"
            )));
        }
        out.insert(k.trim().to_owned(), v.trim_matches('"').to_owned());
    }
    Ok(out)
}

/// `s` split on whitespace outside double quotes, so a quoted matcher value may hold spaces, or the
/// word `and`, without ending the word it is in (`T-018-13`). An unclosed quote is refused.
fn words(s: &str) -> Result<Vec<String>, Error> {
    let (mut out, mut word, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        if c == '"' {
            quoted = !quoted;
        }
        if c.is_whitespace() && !quoted {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if quoted {
        return Err(Error::Config(format!("{s:?} has an unclosed quote")));
    }
    if !word.is_empty() {
        out.push(word);
    }
    Ok(out)
}

impl Expr {
    /// Parse `metric{label="value",...} op integer`, or several joined by `and`.
    pub fn parse(s: &str) -> Result<Expr, Error> {
        let words = words(s)?;
        if let Some(at) = words.iter().position(|w| w == "and") {
            let (left, right) = (words[..at].join(" "), words[at + 1..].join(" "));
            let mut e = Expr::parse_one(&left)?;
            e.and = Some(Box::new(Expr::parse(&right)?));
            return Ok(e);
        }
        Expr::parse_one(s)
    }

    /// The metrics it reads, in order.
    pub fn metrics(&self) -> Vec<&str> {
        let mut out = vec![self.metric.as_str()];
        let mut next = self.and.as_deref();
        while let Some(e) = next {
            out.push(e.metric.as_str());
            next = e.and.as_deref();
        }
        out
    }

    /// Whether the sample `(name, labels, value)` is this comparison's and holds it.
    fn holds(&self, name: &str, labels: &BTreeMap<String, String>, value: i128) -> bool {
        name == self.metric
            && self.matchers.iter().all(|(k, v)| labels.get(k) == Some(v))
            && self.op.holds(value, self.threshold)
    }

    /// Whether the sample is kept: it holds this comparison, and every one joined by `and` is
    /// held by a sample with the same labels.
    fn keeps(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        value: i128,
        all: &[Sample],
    ) -> bool {
        self.holds(name, labels, value)
            && self.and.as_deref().is_none_or(|e| {
                all.iter()
                    .any(|(n, l, v)| l == labels && e.keeps(n, l, *v, all))
            })
    }

    fn parse_one(s: &str) -> Result<Expr, Error> {
        let bad = || Error::Config(format!("{s:?} is not `metric{{...}} op integer`"));
        let words = words(s)?;
        let mut parts = words.iter().map(String::as_str);
        let (series, op, value) = (
            parts.next().ok_or_else(bad)?,
            parts.next().ok_or_else(bad)?,
            parts.next().ok_or_else(bad)?,
        );
        if parts.next().is_some() {
            return Err(bad());
        }
        let op = match op {
            ">" => Op::Gt,
            ">=" => Op::Ge,
            "<" => Op::Lt,
            "<=" => Op::Le,
            "==" => Op::Eq,
            "!=" => Op::Ne,
            _ => return Err(bad()),
        };
        let (metric, matchers) = match series.split_once('{') {
            None => (series.to_owned(), BTreeMap::new()),
            Some((m, rest)) => (
                m.to_owned(),
                parse_matchers(rest.strip_suffix('}').ok_or_else(bad)?)?,
            ),
        };
        let valid = !metric.is_empty()
            && metric
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
        if !valid {
            return Err(bad());
        }
        Ok(Expr {
            metric,
            matchers,
            op,
            threshold: value.parse().map_err(|_| bad())?,
            and: None,
        })
    }
}

/// A duration in Prometheus's form (`30s`, `5m`, `1h`), in milliseconds.
fn prom_duration(s: &str) -> Result<u64, Error> {
    crate::backup::schedule::duration_ms(s)
}

/// One alerting rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Its name.
    pub alert: String,
    /// Its expression.
    pub expr: Expr,
    /// How long it must hold before it fires, in milliseconds.
    pub for_ms: u64,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
    /// Its annotations.
    pub annotations: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    alert: String,
    expr: String,
    #[serde(default, rename = "for")]
    for_: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    name: String,
    rules: Vec<RawRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRules {
    groups: Vec<RawGroup>,
}

/// A set of rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rules(pub Vec<Rule>);

impl Rules {
    /// Parse a Prometheus rules file, refusing anything outside the evaluated subset.
    pub fn parse(text: &str) -> Result<Rules, Error> {
        let raw: RawRules = serde_yaml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        let mut out = Vec::new();
        for g in raw.groups {
            if g.name.is_empty() {
                return Err(Error::Config("a group without a name".into()));
            }
            for r in g.rules {
                let severity = r.labels.get("severity").map(String::as_str);
                if !matches!(severity, Some("page" | "warning")) {
                    return Err(Error::Config(format!(
                        "{}: severity page or warning",
                        r.alert
                    )));
                }
                if out.iter().any(|x: &Rule| x.alert == r.alert) {
                    return Err(Error::Config(format!("{} twice", r.alert)));
                }
                out.push(Rule {
                    expr: Expr::parse(&r.expr)?,
                    for_ms: r
                        .for_
                        .as_deref()
                        .map(prom_duration)
                        .transpose()?
                        .unwrap_or(0),
                    alert: r.alert,
                    labels: r.labels,
                    annotations: r.annotations,
                });
            }
        }
        Ok(Rules(out))
    }

    /// The committed rules.
    pub fn committed() -> Result<Rules, Error> {
        Rules::parse(RULES)
    }

    /// The rule named `alert`.
    pub fn get(&self, alert: &str) -> Option<&Rule> {
        self.0.iter().find(|r| r.alert == alert)
    }
}

/// A sample: a metric, its labels, its value.
pub type Sample = (String, BTreeMap<String, String>, i128);

/// An alert that fires: its rule's name and the labels it carries (the rule's over the sample's).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Alert {
    /// The rule.
    pub alert: String,
    /// Its labels, with `alertname`.
    pub labels: BTreeMap<String, String>,
}

/// Rules evaluated over time: a condition fires once it has held for its rule's `for`.
#[derive(Clone, Debug)]
pub struct Evaluator {
    rules: Rules,
    pending: BTreeMap<(String, BTreeMap<String, String>), u64>,
}

impl Evaluator {
    /// An evaluator of `rules`.
    pub fn new(rules: Rules) -> Evaluator {
        Evaluator {
            rules,
            pending: BTreeMap::new(),
        }
    }

    /// Evaluate `samples` at `now`: the alerts firing.
    pub fn step(&mut self, now: u64, samples: &[Sample]) -> Vec<Alert> {
        let mut firing = Vec::new();
        let mut held = BTreeMap::new();
        for rule in &self.rules.0 {
            for (name, labels, value) in samples {
                if !rule.expr.keeps(name, labels, *value, samples) {
                    continue;
                }
                let key = (rule.alert.clone(), labels.clone());
                let since = self.pending.get(&key).copied().unwrap_or(now);
                held.insert(key, since);
                if now.saturating_sub(since) >= rule.for_ms {
                    let mut l = labels.clone();
                    l.extend(rule.labels.clone());
                    l.insert("alertname".into(), rule.alert.clone());
                    firing.push(Alert {
                        alert: rule.alert.clone(),
                        labels: l,
                    });
                }
            }
        }
        self.pending = held;
        firing.sort();
        firing.dedup();
        firing
    }
}

/// A node of the routing tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// Its receiver.
    pub receiver: String,
    /// What it matches: label equalities.
    pub matchers: BTreeMap<String, String>,
    /// Its children, tried in order.
    pub routes: Vec<Route>,
}

#[derive(Deserialize)]
struct RawRoute {
    receiver: Option<String>,
    #[serde(default)]
    matchers: Vec<String>,
    #[serde(default)]
    routes: Vec<RawRoute>,
    #[serde(default, rename = "continue")]
    cont: bool,
}

#[derive(Deserialize)]
struct RawReceiver {
    name: String,
    #[serde(default)]
    webhook_configs: Vec<serde_yaml::Value>,
}

#[derive(Deserialize)]
struct RawRouting {
    route: RawRoute,
    receivers: Vec<RawReceiver>,
}

fn route_of(r: RawRoute, parent: &str) -> Result<Route, Error> {
    if r.cont {
        return Err(Error::Config(
            "`continue` is not in the evaluated subset".into(),
        ));
    }
    let receiver = r.receiver.unwrap_or_else(|| parent.to_owned());
    let mut matchers = BTreeMap::new();
    for m in &r.matchers {
        matchers.extend(parse_matchers(m)?);
    }
    let routes = r
        .routes
        .into_iter()
        .map(|c| route_of(c, &receiver))
        .collect::<Result<_, _>>()?;
    Ok(Route {
        receiver,
        matchers,
        routes,
    })
}

/// The routing tree and its receivers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routing {
    /// The root.
    pub route: Route,
    /// Every receiver's name, and whether it notifies anyone.
    pub receivers: BTreeMap<String, bool>,
}

impl Routing {
    /// Parse an Alertmanager configuration. A receiver's webhook must name its URL by file:
    /// no endpoint is committed.
    pub fn parse(text: &str) -> Result<Routing, Error> {
        let raw: RawRouting =
            serde_yaml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        let mut receivers = BTreeMap::new();
        for r in raw.receivers {
            for w in &r.webhook_configs {
                if w.get("url").is_some() || w.get("url_file").is_none() {
                    return Err(Error::Config(format!(
                        "{}: a webhook names its URL by url_file, never inline",
                        r.name
                    )));
                }
            }
            receivers.insert(r.name, !r.webhook_configs.is_empty());
        }
        let route = route_of(raw.route, "")?;
        if route.receiver.is_empty() {
            return Err(Error::Config("the root route has no receiver".into()));
        }
        Ok(Routing { route, receivers })
    }

    /// The committed routes.
    pub fn committed() -> Result<Routing, Error> {
        Routing::parse(ROUTES)
    }

    /// The receiver an alert with `labels` reaches: the first matching child, recursively, else
    /// the node's own.
    pub fn receiver(&self, labels: &BTreeMap<String, String>) -> &str {
        let mut node = &self.route;
        'down: loop {
            for c in &node.routes {
                if c.matchers.iter().all(|(k, v)| labels.get(k) == Some(v)) {
                    node = c;
                    continue 'down;
                }
            }
            return &node.receiver;
        }
    }
}

/// A receiver that records what reaches it: the test route standing for the pager.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TestRoute {
    /// Every alert delivered, in order.
    pub delivered: Vec<Alert>,
}

impl TestRoute {
    /// Deliver `alerts` routed by `routing`: those reaching `receiver` are kept.
    pub fn deliver(&mut self, routing: &Routing, receiver: &str, alerts: &[Alert]) {
        for a in alerts {
            if routing.receiver(&a.labels) == receiver {
                self.delivered.push(a.clone());
            }
        }
    }

    /// Whether `alert` reached it.
    pub fn got(&self, alert: &str) -> bool {
        self.delivered.iter().any(|a| a.alert == alert)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_rules_and_routes_parse_in_the_evaluated_subset() {
        let rules = Rules::committed().unwrap();
        assert!(rules.0.len() >= 20);
        let routing = Routing::committed().unwrap();
        assert_eq!(routing.receivers.get("pager"), Some(&true));
        assert_eq!(routing.receivers.get("dashboard"), Some(&false));
        assert!(Expr::parse("a_b{x=~\"y\"} > 1").is_err());
        assert!(Expr::parse("rate(a[5m]) > 1").is_err());
        assert!(Expr::parse("a > 1.5").is_err());
        assert!(Expr::parse("a > 1 or b > 0").is_err());
        assert!(Expr::parse("a > 1 and").is_err());
        let both = Expr::parse("a > 1 and b{x=\"y\"} > 0").unwrap();
        assert_eq!(both.metrics(), ["a", "b"]);
        let inline = ROUTES.replace("url_file:", "url:");
        assert!(Routing::parse(&inline).is_err());
    }

    /// `and` inside a quoted matcher value is the value's, not the joining word (`T-018-13`).
    #[test]
    fn t_018_13_and_inside_a_quoted_value_is_not_a_join() {
        let one = Expr::parse("a{kind=\"stand and deliver\"} > 0").unwrap();
        assert_eq!(one.metrics(), ["a"]);
        assert_eq!(
            one.matchers.get("kind").map(String::as_str),
            Some("stand and deliver")
        );
        assert!(one.and.is_none());
        let word = Expr::parse("a{kind=\"and\"} > 0 and b{x=\"y and z\"} > 1").unwrap();
        assert_eq!(word.metrics(), ["a", "b"]);
        assert_eq!(word.matchers.get("kind").map(String::as_str), Some("and"));
        let right = word.and.as_deref().unwrap();
        assert_eq!(right.matchers.get("x").map(String::as_str), Some("y and z"));
        assert_eq!(right.threshold, 1);
        assert!(Expr::parse("a{kind=\"and\"} > 0 and").is_err());
        assert!(Expr::parse("a{kind=\"open} > 0").is_err());
    }
}
