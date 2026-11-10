//! CVSS 4.0 **base** vectors (CVSS-B): all eleven base metrics are required, in the
//! specification's order, `CVSS:4.0/AV:…/AC:…/AT:…/PR:…/UI:…/VC:…/VI:…/VA:…/SC:…/SI:…/SA:…`,
//! which display and serde use too. Threat, environmental, supplemental, and unknown
//! metrics are rejected, never ignored.
//!
//! Scores follow the [FIRST specification](https://www.first.org/cvss/v4-0/specification-document)
//! and its reference calculator: the vector's macrovector score, less the mean
//! proportional severity distance to the macrovector's highest-severity vectors. Base
//! scoring takes every threat and environmental metric as unset, which the reference
//! scores as E:A and CR/IR/AR:H. CVSS is owned by FIRST and used by permission.

mod lookup;

use std::{fmt, str::FromStr};

use schemars::JsonSchema;
use serde::Serialize;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use skyhook_base::enums::NamedEnum;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, NamedEnum)]
pub(crate) enum AttackVector {
    #[serde(rename = "N")]
    Network,
    #[serde(rename = "A")]
    Adjacent,
    #[serde(rename = "L")]
    Local,
    #[serde(rename = "P")]
    Physical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, NamedEnum)]
pub(crate) enum AttackComplexity {
    #[serde(rename = "L")]
    Low,
    #[serde(rename = "H")]
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, NamedEnum)]
pub(crate) enum AttackRequirements {
    #[serde(rename = "N")]
    None,
    #[serde(rename = "P")]
    Present,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, NamedEnum)]
pub(crate) enum PrivilegesRequired {
    #[serde(rename = "N")]
    None,
    #[serde(rename = "L")]
    Low,
    #[serde(rename = "H")]
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, NamedEnum)]
pub(crate) enum UserInteraction {
    #[serde(rename = "N")]
    None,
    #[serde(rename = "P")]
    Passive,
    #[serde(rename = "A")]
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, NamedEnum)]
pub(crate) enum Impact {
    #[serde(rename = "H")]
    High,
    #[serde(rename = "L")]
    Low,
    #[serde(rename = "N")]
    None,
}

/// A base metric, declared in the order a vector lists them.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
pub(crate) enum BaseMetric {
    #[serde(rename = "AV")]
    AttackVector,
    #[serde(rename = "AC")]
    AttackComplexity,
    #[serde(rename = "AT")]
    AttackRequirements,
    #[serde(rename = "PR")]
    PrivilegesRequired,
    #[serde(rename = "UI")]
    UserInteraction,
    #[serde(rename = "VC")]
    VulnerableConfidentiality,
    #[serde(rename = "VI")]
    VulnerableIntegrity,
    #[serde(rename = "VA")]
    VulnerableAvailability,
    #[serde(rename = "SC")]
    SubsequentConfidentiality,
    #[serde(rename = "SI")]
    SubsequentIntegrity,
    #[serde(rename = "SA")]
    SubsequentAvailability,
}

/// Severity ordering is the CVSS qualitative scale; the default shortlist threshold is high.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    None,
    Low,
    Medium,
    High,
    Critical,
}

/// The confidentiality, integrity and availability impact on one system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Impacts {
    pub(crate) confidentiality: Impact,
    pub(crate) integrity: Impact,
    pub(crate) availability: Impact,
}

impl Impacts {
    fn none(self) -> bool {
        [self.confidentiality, self.integrity, self.availability] == [Impact::None; 3]
    }

    fn any_high(self) -> bool {
        [self.confidentiality, self.integrity, self.availability].contains(&Impact::High)
    }
}

// A complete base assessment.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr, JsonSchema,
)]
#[schemars(with = "String")]
pub(crate) struct Cvss40 {
    pub(crate) attack_vector: AttackVector,
    pub(crate) attack_complexity: AttackComplexity,
    pub(crate) attack_requirements: AttackRequirements,
    pub(crate) privileges_required: PrivilegesRequired,
    pub(crate) user_interaction: UserInteraction,
    /// Impact on the vulnerable system.
    pub(crate) vulnerable: Impacts,
    /// Impact on systems beyond it.
    pub(crate) subsequent: Impacts,
}

/// A security requirement of the reference's highest-severity vectors; base scoring
/// holds every requirement high.
#[derive(Clone, Copy)]
enum Requirement {
    High,
    Medium,
}

/// Severity levels: each step down a metric's scale adds a tenth.
trait Level {
    fn level(self) -> f64;
}

macro_rules! levels {
    ($($name:ty { $($variant:ident => $level:literal),+ })+) => {
        $(impl Level for $name {
            fn level(self) -> f64 {
                match self {
                    $(Self::$variant => $level,)+
                }
            }
        })+
    };
}

levels! {
    AttackVector { Network => 0.0, Adjacent => 0.1, Local => 0.2, Physical => 0.3 }
    PrivilegesRequired { None => 0.0, Low => 0.1, High => 0.2 }
    UserInteraction { None => 0.0, Passive => 0.1, Active => 0.2 }
    AttackComplexity { Low => 0.0, High => 0.1 }
    AttackRequirements { None => 0.0, Present => 0.1 }
    Requirement { High => 0.0, Medium => 0.1 }
}

impl Impact {
    fn vulnerable_level(self) -> f64 {
        match self {
            Self::High => 0.0,
            Self::Low => 0.1,
            Self::None => 0.2,
        }
    }

    /// Subsequent integrity and availability also rank safety above high; a base
    /// vector never carries it.
    fn subsequent_level(self) -> f64 {
        match self {
            Self::High => 0.1,
            Self::Low => 0.2,
            Self::None => 0.3,
        }
    }
}

/// The equivalence-class digits EQ1..EQ6 of a vector's macrovector.
type Macrovector = [u8; 6];

fn macrovector_score(macrovector: Macrovector) -> Option<f64> {
    let key = macrovector.map(|digit| b'0' + digit);
    let key = std::str::from_utf8(&key).ok()?;
    let index = lookup::LOOKUP.binary_search_by(|(entry, _)| (*entry).cmp(key));
    index.ok().map(|index| lookup::LOOKUP[index].1)
}

use AttackComplexity as Ac;
use AttackRequirements as At;
use AttackVector as Av;
use Impact::{High as H, Low as L};
use PrivilegesRequired as Pr;
use Requirement::{High as RH, Medium as RM};
use UserInteraction as Ui;

/// Each EQ's highest-severity vectors, from the reference's `max_composed.js`.
const EQ1_MAXES: [&[(AttackVector, PrivilegesRequired, UserInteraction)]; 3] = [
    &[(Av::Network, Pr::None, Ui::None)],
    &[
        (Av::Adjacent, Pr::None, Ui::None),
        (Av::Network, Pr::Low, Ui::None),
        (Av::Network, Pr::None, Ui::Passive),
    ],
    &[
        (Av::Physical, Pr::None, Ui::None),
        (Av::Adjacent, Pr::Low, Ui::Passive),
    ],
];
const EQ2_MAXES: [&[(AttackComplexity, AttackRequirements)]; 2] = [
    &[(Ac::Low, At::None)],
    &[(Ac::High, At::None), (Ac::Low, At::Present)],
];
/// Vulnerable-system impact (VC, VI, VA) and requirements (CR, IR, AR), by EQ3 and EQ6.
type ImpactMax = ([Impact; 3], [Requirement; 3]);
fn eq3_eq6_maxes(eq3: u8, eq6: u8) -> &'static [ImpactMax] {
    match (eq3, eq6) {
        (0, 0) => &[([H, H, H], [RH, RH, RH])],
        (0, _) => &[([H, H, L], [RM, RM, RH]), ([H, H, H], [RM, RM, RM])],
        (1, 0) => &[([L, H, H], [RH, RH, RH]), ([H, L, H], [RH, RH, RH])],
        (1, _) => &[
            ([L, H, L], [RH, RM, RH]),
            ([L, H, H], [RH, RM, RM]),
            ([H, L, H], [RM, RH, RM]),
            ([H, L, L], [RM, RH, RH]),
            ([L, L, H], [RH, RH, RM]),
        ],
        _ => &[([L, L, L], [RH, RH, RH])],
    }
}
/// Subsequent-system impact (SC, SI, SA) by EQ4; base EQ4 is 1 or 2.
fn eq4_max(eq4: u8) -> [Impact; 3] {
    if eq4 == 1 { [H, H, H] } else { [L, L, L] }
}

/// The reference's maximal severity distance within each EQ's macrovector, in steps.
fn depths([eq1, eq2, eq3, eq4, _, eq6]: Macrovector) -> [u8; 4] {
    let eq3_eq6 = match (eq3, eq6) {
        (0, 0) => 7,
        (0, _) => 6,
        (1, _) => 8,
        _ => 10,
    };
    [
        [1, 4, 5][usize::from(eq1)],
        [1, 2][usize::from(eq2)],
        eq3_eq6,
        [6, 5, 4][usize::from(eq4)],
    ]
}

impl Cvss40 {
    fn macrovector(&self) -> Macrovector {
        let (av, pr, ui) = (
            self.attack_vector,
            self.privileges_required,
            self.user_interaction,
        );
        let eq1 = if (av, pr, ui) == (Av::Network, Pr::None, Ui::None) {
            0
        } else if (av == Av::Network || pr == Pr::None || ui == Ui::None) && av != Av::Physical {
            1
        } else {
            2
        };
        let eq2 =
            u8::from((self.attack_complexity, self.attack_requirements) != (Ac::Low, At::None));
        let v = self.vulnerable;
        let eq3 = if (v.confidentiality, v.integrity) == (H, H) {
            0
        } else if v.any_high() {
            1
        } else {
            2
        };
        let eq4 = if self.subsequent.any_high() { 1 } else { 2 };
        // Every requirement is high, so EQ6 asks only whether a vulnerable impact is.
        let eq6 = u8::from(!v.any_high());
        [eq1, eq2, eq3, eq4, 0, eq6]
    }

    /// Per-EQ severity distances (EQ1, EQ2, EQ3+EQ6, EQ4) from the first
    /// highest-severity vector this one does not exceed, or from the last when none
    /// qualifies, as the reference does.
    fn distances(&self, [eq1, eq2, eq3, eq4, _, eq6]: Macrovector) -> [f64; 4] {
        let mut last = [0.0; 4];
        for &(av, pr, ui) in EQ1_MAXES[usize::from(eq1)] {
            for &(ac, at) in EQ2_MAXES[usize::from(eq2)] {
                for &(vulnerable, requirements) in eq3_eq6_maxes(eq3, eq6) {
                    let subsequent = eq4_max(eq4);
                    let parts = [
                        self.attack_vector.level() - av.level(),
                        self.privileges_required.level() - pr.level(),
                        self.user_interaction.level() - ui.level(),
                        self.attack_complexity.level() - ac.level(),
                        self.attack_requirements.level() - at.level(),
                        self.vulnerable.confidentiality.vulnerable_level()
                            - vulnerable[0].vulnerable_level(),
                        self.vulnerable.integrity.vulnerable_level()
                            - vulnerable[1].vulnerable_level(),
                        self.vulnerable.availability.vulnerable_level()
                            - vulnerable[2].vulnerable_level(),
                        self.subsequent.confidentiality.subsequent_level()
                            - subsequent[0].subsequent_level(),
                        self.subsequent.integrity.subsequent_level()
                            - subsequent[1].subsequent_level(),
                        self.subsequent.availability.subsequent_level()
                            - subsequent[2].subsequent_level(),
                        RH.level() - requirements[0].level(),
                        RH.level() - requirements[1].level(),
                        RH.level() - requirements[2].level(),
                    ];
                    // The reference adds in this order; keep it for identical rounding.
                    last = [
                        parts[0] + parts[1] + parts[2],
                        parts[3] + parts[4],
                        parts[5] + parts[6] + parts[7] + parts[11] + parts[12] + parts[13],
                        parts[8] + parts[9] + parts[10],
                    ];
                    if parts.iter().all(|part| *part >= 0.0) {
                        return last;
                    }
                }
            }
        }
        last
    }

    /// Base score in 0.0–10.0, rounded to one decimal place per CVSS 4.0.
    pub(crate) fn score(&self) -> f64 {
        if self.vulnerable.none() && self.subsequent.none() {
            return 0.0;
        }
        let macrovector = self.macrovector();
        let value = macrovector_score(macrovector).expect("every base macrovector is scored");
        let [eq1, eq2, eq3, eq4, eq5, eq6] = macrovector;
        let lower = |digits: Macrovector| macrovector_score(digits);
        // EQ3 and EQ6 step down together: 00 has two neighbours, the higher one counts.
        let eq3_eq6 = match (eq3, eq6) {
            (0, 0) => {
                let left = lower([eq1, eq2, eq3, eq4, eq5, eq6 + 1]);
                let right = lower([eq1, eq2, eq3 + 1, eq4, eq5, eq6]);
                match (left, right) {
                    (Some(left), Some(right)) if left > right => Some(left),
                    _ => right,
                }
            }
            (1, 0) => lower([eq1, eq2, eq3, eq4, eq5, eq6 + 1]),
            (2, _) => None,
            _ => lower([eq1, eq2, eq3 + 1, eq4, eq5, eq6]),
        };
        let lower_scores = [
            lower([eq1 + 1, eq2, eq3, eq4, eq5, eq6]),
            lower([eq1, eq2 + 1, eq3, eq4, eq5, eq6]),
            eq3_eq6,
            lower([eq1, eq2, eq3, eq4 + 1, eq5, eq6]),
        ];
        let distances = self.distances(macrovector);
        let depths = depths(macrovector);
        let (mut total, mut counted) = (0.0, 0u8);
        for ((lower, distance), depth) in lower_scores.into_iter().zip(distances).zip(depths) {
            if let Some(lower) = lower {
                counted += 1;
                total += (value - lower) * (distance / (f64::from(depth) * 0.1));
            }
        }
        // EQ5 (threat) is always at its highest without threat metrics: its lower
        // neighbour, which every base macrovector has, counts toward the mean with no
        // distance.
        let mean = total / f64::from(counted + 1);
        // Math.round in the reference: half rounds up.
        ((value - mean).clamp(0.0, 10.0) * 10.0 + 0.5).floor() / 10.0
    }

    /// The code `metric` takes in this vector.
    pub(crate) fn code(&self, metric: BaseMetric) -> &'static str {
        let (v, s) = (self.vulnerable, self.subsequent);
        match metric {
            BaseMetric::AttackVector => self.attack_vector.as_str(),
            BaseMetric::AttackComplexity => self.attack_complexity.as_str(),
            BaseMetric::AttackRequirements => self.attack_requirements.as_str(),
            BaseMetric::PrivilegesRequired => self.privileges_required.as_str(),
            BaseMetric::UserInteraction => self.user_interaction.as_str(),
            BaseMetric::VulnerableConfidentiality => v.confidentiality.as_str(),
            BaseMetric::VulnerableIntegrity => v.integrity.as_str(),
            BaseMetric::VulnerableAvailability => v.availability.as_str(),
            BaseMetric::SubsequentConfidentiality => s.confidentiality.as_str(),
            BaseMetric::SubsequentIntegrity => s.integrity.as_str(),
            BaseMetric::SubsequentAvailability => s.availability.as_str(),
        }
    }

    pub(crate) fn severity(&self) -> Severity {
        match self.score() {
            0.0 => Severity::None,
            score if score < 4.0 => Severity::Low,
            score if score < 7.0 => Severity::Medium,
            score if score < 9.0 => Severity::High,
            _ => Severity::Critical,
        }
    }
}

impl fmt::Display for Cvss40 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CVSS:4.0")?;
        for metric in BaseMetric::ALL {
            write!(f, "/{metric}:{}", self.code(metric))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum ParseError {
    #[error("expected a CVSS:4.0/ base vector")]
    InvalidPrefix,
    #[error("expected each metric in NAME:VALUE form")]
    MalformedMetric,
    /// Also covers threat, environmental and supplemental metrics: this parser is base-only.
    #[error(
        "the vector names a metric that is not a CVSS 4.0 base metric; only AV, AC, AT, PR, UI, VC, VI, VA, SC, SI, SA are supported"
    )]
    UnknownMetric,
    #[error("duplicate CVSS base metric {0}")]
    DuplicateMetric(BaseMetric),
    #[error(
        "CVSS base metric {0} is out of order; a CVSS 4.0 vector lists AV, AC, AT, PR, UI, VC, VI, VA, SC, SI, SA in that order"
    )]
    OutOfOrder(BaseMetric),
    #[error("missing CVSS base metric {0}")]
    MissingMetric(BaseMetric),
    #[error("invalid value for CVSS base metric {0}")]
    InvalidValue(BaseMetric),
}

impl FromStr for Cvss40 {
    type Err = ParseError;

    fn from_str(vector: &str) -> Result<Self, Self::Err> {
        let body = vector
            .strip_prefix("CVSS:4.0/")
            .ok_or(ParseError::InvalidPrefix)?;
        let mut metrics = [None; BaseMetric::ALL.len()];
        // The specification makes every ordering but Table 23's invalid, and the enum
        // declares the metrics in that order.
        let mut last = None;
        for token in body.split('/') {
            let (name, value) = token.split_once(':').ok_or(ParseError::MalformedMetric)?;
            let metric = (name.parse::<BaseMetric>()).map_err(|_| ParseError::UnknownMetric)?;
            if metrics[metric as usize].replace(value).is_some() {
                return Err(ParseError::DuplicateMetric(metric));
            }
            if last.is_some_and(|last| last > metric as u8) {
                return Err(ParseError::OutOfOrder(metric));
            }
            last = Some(metric as u8);
        }
        Ok(Self {
            attack_vector: parse_value(&metrics, BaseMetric::AttackVector)?,
            attack_complexity: parse_value(&metrics, BaseMetric::AttackComplexity)?,
            attack_requirements: parse_value(&metrics, BaseMetric::AttackRequirements)?,
            privileges_required: parse_value(&metrics, BaseMetric::PrivilegesRequired)?,
            user_interaction: parse_value(&metrics, BaseMetric::UserInteraction)?,
            vulnerable: Impacts {
                confidentiality: parse_value(&metrics, BaseMetric::VulnerableConfidentiality)?,
                integrity: parse_value(&metrics, BaseMetric::VulnerableIntegrity)?,
                availability: parse_value(&metrics, BaseMetric::VulnerableAvailability)?,
            },
            subsequent: Impacts {
                confidentiality: parse_value(&metrics, BaseMetric::SubsequentConfidentiality)?,
                integrity: parse_value(&metrics, BaseMetric::SubsequentIntegrity)?,
                availability: parse_value(&metrics, BaseMetric::SubsequentAvailability)?,
            },
        })
    }
}

fn parse_value<T: NamedEnum>(
    metrics: &[Option<&str>; BaseMetric::ALL.len()],
    metric: BaseMetric,
) -> Result<T, ParseError> {
    let value = metrics[metric as usize].ok_or(ParseError::MissingMetric(metric))?;
    T::parse(value).ok_or(ParseError::InvalidValue(metric))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRITICAL: &str = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";

    #[test]
    fn scores_and_severities_match_first_examples() {
        // Base vectors from FIRST's published examples (https://www.first.org/cvss/v4.0/examples)
        // at each severity band and its edges, interpolated ones among them. Those examples
        // score none Low, so the Low row is the FIRST calculator's score.
        use Severity::{Critical, High, Low, Medium};
        for (vector, score, severity) in [
            (CRITICAL, 9.3, Critical),
            (
                "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:N/VI:N/VA:H/SC:N/SI:N/SA:L",
                8.7,
                High,
            ),
            (
                "CVSS:4.0/AV:P/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N",
                7.0,
                High,
            ),
            (
                "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:N/VI:N/VA:N/SC:L/SI:L/SA:N",
                6.9,
                Medium,
            ),
            (
                "CVSS:4.0/AV:L/AC:L/AT:N/PR:N/UI:A/VC:L/VI:N/VA:N/SC:N/SI:N/SA:N",
                4.6,
                Medium,
            ),
            (
                "CVSS:4.0/AV:N/AC:L/AT:P/PR:L/UI:A/VC:L/VI:N/VA:N/SC:N/SI:N/SA:N",
                2.0,
                Low,
            ),
            (
                "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:N/VI:N/VA:N/SC:N/SI:N/SA:N",
                0.0,
                Severity::None,
            ),
        ] {
            let cvss: Cvss40 = vector.parse().unwrap();
            assert_eq!(cvss.score(), score, "{vector}");
            assert_eq!(cvss.severity(), severity, "{vector}");
        }
    }

    #[test]
    fn vector_and_serde_round_trip_the_canonical_order() {
        let cvss: Cvss40 = CRITICAL.parse().unwrap();
        assert_eq!(cvss.attack_vector, AttackVector::Network);
        assert_eq!(cvss.privileges_required, PrivilegesRequired::None);
        assert_eq!(cvss.to_string(), CRITICAL);
        let encoded = serde_json::to_string(&cvss).unwrap();
        assert_eq!(encoded, format!("\"{CRITICAL}\""));
        assert_eq!(serde_json::from_str::<Cvss40>(&encoded).unwrap(), cvss);
    }

    #[test]
    fn parsing_rejects_each_malformed_shape() {
        for (vector, error) in [
            (
                CRITICAL.replace("CVSS:4.0", "CVSS:3.1"),
                ParseError::InvalidPrefix,
            ),
            (
                CRITICAL.replace("/AT:N", ""),
                ParseError::MissingMetric(BaseMetric::AttackRequirements),
            ),
            (
                format!("{CRITICAL}/AV:L"),
                ParseError::DuplicateMetric(BaseMetric::AttackVector),
            ),
            (
                "CVSS:4.0/SA:N/SI:N/SC:N/VA:H/VI:H/VC:H/UI:N/PR:N/AT:N/AC:L/AV:N".into(),
                ParseError::OutOfOrder(BaseMetric::SubsequentIntegrity),
            ),
            (
                CRITICAL.replace("AC:L/AT:N", "AT:N/AC:L"),
                ParseError::OutOfOrder(BaseMetric::AttackComplexity),
            ),
            (
                CRITICAL.replace("SI:N", "SI:S"),
                ParseError::InvalidValue(BaseMetric::SubsequentIntegrity),
            ),
            (format!("{CRITICAL}/E:A"), ParseError::UnknownMetric),
            (format!("{CRITICAL}/AV=N"), ParseError::MalformedMetric),
        ] {
            assert_eq!(vector.parse::<Cvss40>(), Err(error), "{vector}");
        }
    }
}
