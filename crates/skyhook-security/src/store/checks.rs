//! Contracts and their checks: whether one function of a component keeps its contract of one
//! kind across calls, asked by question units, one open at a time. Where a contract stands is
//! derived (`contract_standing`), and so is the check it is owed once something bears on it
//! after its latest one ended (`contract_trigger`, the `check` queue): a check planned for it
//! while owed is the run's, so the agent working the queue entry may complete it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, params};

use super::*;

/// The contract a check asks about, with the function producing what it is about and the input
/// it consumes, when known.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Check {
    pub contract: ContractId,
    pub component: ComponentId,
    pub function: String,
    pub kind: InvariantKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewCheck {
    #[serde(default)]
    pub contract: Option<ContractId>,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub function: Option<Text>,
    #[serde(default)]
    pub kind: Option<InvariantKind>,
    pub question: Text,
    pub stopping: Text,
    #[serde(default)]
    pub producer: Option<Text>,
    #[serde(default)]
    pub input: Option<Text>,
    #[serde(default)]
    pub guarantees: Vec<GuaranteeId>,
}

/// A contract by its latest check.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Contract {
    pub id: ContractId,
    pub function: String,
    pub kind: InvariantKind,
    pub state: ContractState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<UnitId>,
}

/// Contract `c` of `contract_standing` is owed a check: untrusted input reaches its component,
/// no flaw broke it, and something bore on it after its latest check ended, or before any.
macro_rules! owed {
    () => {
        concat!(
            "c.state!='broken' AND ",
            components::untrusted_reach!("c.component"),
            " AND (c.unit IS NULL OR c.ended IS NOT NULL) AND EXISTS (SELECT 1 FROM \
             contract_trigger t WHERE t.contract=c.id AND t.event>coalesce(c.ended,0))"
        )
    };
}
pub(super) use owed;

/// A check `actor` plans: the contract's open check, which takes its guarantees too, or a new
/// one, the run's while the contract is owed one; refused unless untrusted input reaches the
/// component, and once a flaw broke the contract.
pub(super) fn add(db: &Db, actor: &AgentId, input: &NewCheck) -> Result<UnitId> {
    let contract = match (input.contract, input.component, &input.function, input.kind) {
        (Some(contract), None, None, None) => {
            scope::known(db, contract)?;
            contract
        }
        (None, Some(component), Some(function), Some(kind)) => {
            scope::known(db, component)?;
            db.execute(
                "INSERT INTO contract(component,function,kind) VALUES (?1,?2,?3) ON CONFLICT DO \
                 NOTHING",
                params![component.0, function, kind],
            )?;
            stored(
                db,
                "SELECT id FROM contract WHERE component=?1 AND function=?2 AND kind=?3",
                params![component.0, function, kind],
                |row| Ok(ContractId(row.get(0)?)),
            )?
        }
        _ => return Err(Error::ContractKey),
    };
    let (component, function, kind, reached, owed, open, state) = stored(
        db,
        concat!(
            "SELECT c.component,c.function,c.kind,",
            components::untrusted_reach!("c.component"),
            ",",
            owed!(),
            ",CASE WHEN c.ended IS NULL THEN c.unit END,c.state FROM contract_standing c WHERE \
             c.id=?1"
        ),
        params![contract.0],
        |row| {
            Ok((
                ComponentId(row.get(0)?),
                row.get::<String>(1)?,
                enum_column::<InvariantKind>(row, 2)?,
                row.get::<i64>(3)? != 0,
                row.get::<i64>(4)? != 0,
                row.get::<Option<i64>>(5)?.map(UnitId),
                enum_column::<ContractState>(row, 6)?,
            ))
        },
    )?;
    if !reached {
        return Err(Error::Unreached { component });
    }
    if input.guarantees.len() > MAX_ATTACHMENTS {
        return Err(Error::Limit(Bound::Attachments));
    }
    let unit = match (state, open) {
        (ContractState::Broken, _) => return Err(Error::Broken { contract }),
        (_, Some(unit)) => unit,
        (_, None) => {
            let work = UnitWork::Question {
                question: input.question.clone(),
                stopping: input.stopping.clone(),
                invariants: Vec::new(),
                check: Some(Check {
                    contract,
                    component,
                    function,
                    kind,
                    producer: input.producer.as_ref().map(ToString::to_string),
                    input: input.input.as_ref().map(ToString::to_string),
                }),
            };
            match owed {
                true => plan::owed(db, actor, &work)?,
                false => plan::insert(db, actor, &work)?,
            }
        }
    };
    for guarantee in &input.guarantees {
        scope::known(db, *guarantee)?;
        db.execute(
            "INSERT INTO unit_invariant(unit,guarantee) VALUES (?1,?2) ON CONFLICT DO NOTHING",
            params![unit.0, guarantee.0],
        )?;
    }
    Ok(unit)
}

/// What bore on check `unit`'s contract after it was planned.
pub(super) fn since(db: &Db, unit: UnitId) -> Result<Vec<EventRecord>> {
    rows(
        db,
        "SELECT t.retry_site,t.undermining,t.evidence FROM unit_check k JOIN contract_trigger t ON \
         t.contract=k.contract WHERE k.unit=?1 AND t.event>(SELECT event FROM event_record WHERE \
         unit=?1) GROUP BY t.retry_site,t.undermining,t.evidence ORDER BY max(t.event)",
        params![unit.0],
        |row| {
            let id = |index| row.get::<Option<i64>>(index);
            Ok(match (id(0)?, id(1)?, id(2)?) {
                (Some(site), _, _) => EventRecord::RetrySite(RetrySiteId(site)),
                (_, Some(undermining), _) => EventRecord::Undermining(UnderminingId(undermining)),
                (_, _, Some(evidence)) => EventRecord::Evidence(EvidenceId(evidence)),
                (None, None, None) => unreachable!("a trigger names its record"),
            })
        },
    )
}

/// Each of `component`'s contracts by its latest check.
pub(super) fn of_component(db: &Db, component: ComponentId) -> Result<Vec<Contract>> {
    rows(
        db,
        "SELECT id,function,kind,state,unit FROM contract_standing WHERE component=?1 ORDER BY \
         function,kind",
        params![component.0],
        |row| {
            Ok(Contract {
                id: ContractId(row.get(0)?),
                function: row.get(1)?,
                kind: enum_column(row, 2)?,
                state: enum_column(row, 3)?,
                unit: row.get::<Option<i64>>(4)?.map(UnitId),
            })
        },
    )
}

impl Store {
    /// A contract check `actor` plans ([`add`]).
    pub fn add_check(&self, actor: &AgentId, input: &NewCheck) -> Result<UnitId> {
        self.write(|db| add(db, actor, input))
    }
}

#[cfg(test)]
mod tests {
    use skyhook::identity::AgentId;

    use super::{
        super::tests::{
            answerer, component, evidence_at, fixture, location, new_flaw, new_guarantee, path,
            queued, reviewed, source, text,
        },
        *,
    };

    fn check(component: ComponentId, function: &str, kind: InvariantKind) -> NewCheck {
        NewCheck {
            contract: None,
            component: Some(component),
            function: Some(text(function)),
            kind: Some(kind),
            question: text("can one call break it?"),
            stopping: text("a guarantee that holds, a flaw, or what is missing"),
            producer: None,
            input: None,
            guarantees: Vec::new(),
        }
    }

    fn state(store: &Store, component: ComponentId) -> Vec<ContractState> {
        let contracts = store.dossier(component).unwrap().checks;
        contracts.iter().map(|contract| contract.state).collect()
    }

    /// Retry sites meaning invalid input or another failure owe their function one status
    /// check once untrusted input reaches the component and its sweep ended; the check planned
    /// for it is the run's and settles the entry, and the agent that recorded a site never
    /// completes it.
    #[test]
    fn a_retry_site_owes_its_function_one_status_check_where_untrusted_input_reaches() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let operator = source(&store, &actor, "operator", Trust::Privileged);
        let trusted = NewPath {
            description: text("operator"),
            sources: vec![operator],
            reaches: vec![Reach {
                component: decoder,
                note: text("direct call"),
            }],
        };
        store.add_path(&actor, &trusted).unwrap();
        let recorder = AgentId::root(actor.session()).child(5);
        let site = |line, function: &str, class| NewRetrySite {
            location: location("decoder/lib.c", line).in_function(text(function)),
            status: text("NEEDS_MORE_INPUT"),
            producer: None,
            class: Some(class),
        };
        for (line, function, class) in [
            (10, "DecodeStream", RetryClass::Invalid),
            (20, "DecodeStream", RetryClass::Other),
            (30, "ReadInput", RetryClass::InputExhausted),
        ] {
            (store.record_retry_site(&recorder, &site(line, function, class))).unwrap();
        }
        let owed = || queued(&store, Phase::Check);
        assert!(owed().is_empty(), "no untrusted input reaches it");
        path(&store, &actor, "stream", &[decoder]);
        assert!(owed().is_empty(), "its sweep is open");
        reviewed(&store, &actor);
        let [WorkSubject::Contract(contract)] = owed()[..] else {
            panic!("one status check owed")
        };
        let status = NewCheck {
            contract: Some(contract),
            component: None,
            function: None,
            kind: None,
            ..check(decoder, "DecodeStream", InvariantKind::StatusContract)
        };
        let unit = store.add_check(&recorder, &status).unwrap();
        assert!(owed().is_empty(), "the planned check settles it");
        let complete = |who: &AgentId| {
            store.record_outcome(who, unit, Outcome::Completed, &text("checked"), &[])
        };
        assert!(matches!(complete(&recorder), Err(Error::OwnUnit { .. })));
        let evidence = [10, 20, 30]
            .map(|line| evidence_at("decoder/lib.c", line))
            .to_vec();
        let reports = new_guarantee(InvariantKind::StatusContract, evidence);
        let reports = store.add_guarantee(&actor, decoder, &reports).unwrap();
        let named = NewCheck {
            guarantees: vec![reports],
            ..check(decoder, "DecodeStream", InvariantKind::StatusContract)
        };
        assert_eq!(store.add_check(&actor, &named).unwrap(), unit);
        complete(&answerer(&actor)).unwrap();
        assert_eq!(state(&store, decoder), [ContractState::Supported]);
    }

    /// A contract has one open check and stands by its latest: supported while the guarantees
    /// it names hold, owed again once evidence contradicts one, open when it ends
    /// inconclusive, and broken for good once a flaw undermines one.
    #[test]
    fn a_contract_has_one_open_check_and_stands_by_its_latest() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let bounds = |guarantees| NewCheck {
            guarantees,
            ..check(decoder, "DecodeStream", InvariantKind::Bounds)
        };
        assert!(matches!(
            store.add_check(&actor, &bounds(Vec::new())),
            Err(Error::Unreached { .. })
        ));
        path(&store, &actor, "stream", &[decoder]);
        reviewed(&store, &actor);
        let guarantee = new_guarantee(
            InvariantKind::Bounds,
            vec![evidence_at("decoder/lib.c", 40)],
        );
        let bound = store.add_guarantee(&actor, decoder, &guarantee).unwrap();
        let first = store.add_check(&actor, &bounds(Vec::new())).unwrap();
        assert_eq!(
            store.add_check(&actor, &bounds(vec![bound])).unwrap(),
            first
        );
        let answerer = answerer(&actor);
        let end =
            |unit, outcome| store.record_outcome(&answerer, unit, outcome, &text("traced"), &[]);
        end(first, Outcome::Completed).unwrap();
        assert_eq!(state(&store, decoder), [ContractState::Supported]);

        let against = NewEvidence {
            stance: Stance::Contradicts,
            ..evidence_at("decoder/lib.c", 41)
        };
        (store.add_evidence(&actor, EvidenceSubject::Guarantee(bound), &[against])).unwrap();
        assert_eq!(state(&store, decoder), [ContractState::Unsettled]);
        assert_eq!(queued(&store, Phase::Check).len(), 1);
        let second = store.add_check(&actor, &bounds(vec![bound])).unwrap();
        assert_ne!(second, first, "an ended check reopens as a new unit");
        end(second, Outcome::Inconclusive).unwrap();
        assert_eq!(
            store.coverage().unwrap().contracts_in(ContractState::Open),
            1
        );

        let third = store.add_check(&actor, &bounds(vec![bound])).unwrap();
        let overrun = store
            .add_flaw(&actor, &new_flaw(decoder, "a ninth byte"))
            .unwrap();
        let note = text("the retry stages one byte too many");
        (store.undermine(&actor, overrun, Invariant::Guarantee(bound), &note)).unwrap();
        assert_eq!(state(&store, decoder), [ContractState::Broken]);
        assert!(matches!(
            store.unit(third).unwrap().since.as_deref(),
            Some([EventRecord::Undermining(_)])
        ));
        assert!(matches!(
            store.add_check(&actor, &bounds(Vec::new())),
            Err(Error::Broken { .. })
        ));
    }
}
