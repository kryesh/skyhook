//! A realm's KDC from its DNS SRV records, looked up through this machine's resolver.

use std::ffi::{CString, c_char, c_int, c_uchar};

use url::Url;

const CLASS_IN: c_int = 1;
const TYPE_SRV: u16 = 33;
/// The largest DNS message.
const MESSAGE_BYTES: usize = 65_535;
const HEADER_BYTES: usize = 12;
/// More compression pointers than a name of at most 127 labels can need.
const MAX_JUMPS: usize = 128;
const MAX_NAME_BYTES: usize = 255;

unsafe extern "C" {
    /// glibc's and musl's resolver, configured as every other lookup on this machine is.
    fn res_query(
        name: *const c_char,
        class: c_int,
        kind: c_int,
        answer: *mut c_uchar,
        length: c_int,
    ) -> c_int;
}

/// The KDC that `_kerberos._tcp.<realm>` names, else `_kerberos._udp.<realm>`.
pub(super) fn kdc(realm: &str) -> Option<Url> {
    ["tcp", "udp"].into_iter().find_map(|protocol| {
        let answer = query(&format!("_kerberos._{protocol}.{realm}"))?;
        let chosen = choose(records(&answer)?)?;
        Url::parse(&format!("{protocol}://{}:{}", chosen.target, chosen.port)).ok()
    })
}

fn query(name: &str) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    let mut answer = vec![0; MESSAGE_BYTES];
    let length = c_int::try_from(answer.len()).expect("a DNS message fits an int");
    // SAFETY: `name` is NUL-terminated and `answer` is writable for `length` bytes; both
    // outlive the call.
    let written = unsafe {
        res_query(
            name.as_ptr(),
            CLASS_IN,
            c_int::from(TYPE_SRV),
            answer.as_mut_ptr(),
            length,
        )
    };
    answer.truncate(usize::try_from(written).ok()?.min(MESSAGE_BYTES));
    Some(answer)
}

#[derive(Debug, PartialEq, Eq)]
struct Srv {
    priority: u16,
    weight: u16,
    port: u16,
    target: String,
}

/// The record a client tries first: the lowest priority, and of those the heaviest.
/// A target of `.` says the service is not offered.
fn choose(records: Vec<Srv>) -> Option<Srv> {
    (records.into_iter())
        .filter(|record| !record.target.is_empty())
        .min_by_key(|record| (record.priority, std::cmp::Reverse(record.weight)))
}

/// The SRV records among a DNS message's answers; none when it is malformed.
fn records(message: &[u8]) -> Option<Vec<Srv>> {
    let count = |at: usize| Some(usize::from(u16::from_be_bytes(field(message, at)?)));
    let (questions, answers) = (count(4)?, count(6)?);
    let mut at = HEADER_BYTES;
    for _ in 0..questions {
        at = name(message, at)?.1 + 4;
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        let after = name(message, at)?.1;
        let kind = u16::from_be_bytes(field(message, after)?);
        let length = usize::from(u16::from_be_bytes(field(message, after + 8)?));
        let data = after + 10;
        let end = data
            .checked_add(length)
            .filter(|&end| end <= message.len())?;
        if kind == TYPE_SRV {
            let number = |offset| Some(u16::from_be_bytes(field(&message[..end], data + offset)?));
            records.push(Srv {
                priority: number(0)?,
                weight: number(2)?,
                port: number(4)?,
                target: name(&message[..end], data + 6)?.0,
            });
        }
        at = end;
    }
    Some(records)
}

fn field<const N: usize>(message: &[u8], at: usize) -> Option<[u8; N]> {
    message.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// The name at `at`, its labels joined by dots (the root is empty), and where the
/// bytes after it start, following compression pointers into earlier bytes.
fn name(message: &[u8], mut at: usize) -> Option<(String, usize)> {
    let (mut labels, mut after, mut jumps) = (Vec::new(), None, 0);
    loop {
        let length = *message.get(at)?;
        match length {
            0 => break,
            0xC0..=0xFF => {
                let [high, low] = field(message, at)?;
                after.get_or_insert(at + 2);
                jumps += 1;
                if jumps > MAX_JUMPS {
                    return None;
                }
                at = usize::from(u16::from_be_bytes([high & 0x3F, low]));
            }
            1..=0x3F => {
                let label = message.get(at + 1..at + 1 + usize::from(length))?;
                labels.push(String::from_utf8_lossy(label).into_owned());
                at += 1 + usize::from(length);
            }
            _ => return None,
        }
    }
    let name = labels.join(".");
    (name.len() <= MAX_NAME_BYTES).then(|| (name, after.unwrap_or(at + 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(name: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        for label in name.split('.').filter(|label| !label.is_empty()) {
            bytes.push(u8::try_from(label.len()).unwrap());
            bytes.extend_from_slice(label.as_bytes());
        }
        bytes
    }

    /// An answer to `_kerberos._tcp.lab` holding `records`, each a priority, weight,
    /// port and target; a target of `@` points back at the question's `lab`.
    fn answer(records: &[(u16, u16, u16, &str)]) -> Vec<u8> {
        let count = u16::try_from(records.len()).unwrap();
        let mut message = [0, 0, 0x81, 0x80, 0, 1].to_vec();
        message.extend_from_slice(&count.to_be_bytes());
        message.extend_from_slice(&[0, 0, 0, 0]);
        message.extend(labels("_kerberos._tcp.lab"));
        message.extend_from_slice(&[0, 0, 33, 0, 1]);
        for &(priority, weight, port, target) in records {
            let target = match target {
                "@" => vec![0xC0, 12 + 1 + 9 + 1 + 4],
                target => [labels(target), vec![0]].concat(),
            };
            let data = [
                &priority.to_be_bytes()[..],
                &weight.to_be_bytes(),
                &port.to_be_bytes(),
                &target,
            ]
            .concat();
            message.extend_from_slice(&[0xC0, 12, 0, 33, 0, 1, 0, 0, 0, 60]);
            message.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
            message.extend(data);
        }
        message
    }

    /// The record tried first is the lowest priority's heaviest, through compressed
    /// names; one offering nothing is passed over, and a malformed answer has none.
    #[test]
    fn srv_answers_choose_the_lowest_priority_and_heaviest_target() {
        let message = answer(&[
            (10, 100, 88, "backup.lab"),
            (0, 5, 88, "light.lab"),
            (0, 50, 750, "@"),
            (0, 90, 88, ""),
        ]);
        let chosen = choose(records(&message).unwrap()).unwrap();
        assert_eq!((chosen.target.as_str(), chosen.port), ("lab", 750));
        assert_eq!(choose(records(&answer(&[(0, 0, 88, "")])).unwrap()), None);
        assert_eq!(records(&message[..message.len() - 3]), None);
        let mut looping = answer(&[(0, 0, 88, "dc.lab")]);
        let pointer = HEADER_BYTES;
        looping[pointer..pointer + 2].copy_from_slice(&[0xC0, 12]);
        assert_eq!(records(&looping), None);
    }
}
