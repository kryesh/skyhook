//! WS-Management messages for a WinRS shell, and the parts of replies the client reads.

use base64::Engine as _;
use quick_xml::{
    Reader, XmlVersion,
    events::{BytesRef, BytesStart, Event},
};

use super::error::{DecodeError, Fault};

/// How long a Receive waits for output before the service answers that it timed out;
/// shorter than common proxy idle limits.
pub(super) const OPERATION_TIMEOUT_SECS: u64 = 20;
/// How long a shell may go without a request before the service closes it. Receive
/// polls keep a live shell busy, so this bounds how long an abandoned one runs.
const IDLE_TIMEOUT_SECS: u64 = 120;
/// The largest reply asked for: the service default since Windows Server 2012.
pub(super) const MAX_ENVELOPE_BYTES: usize = 500 * 1024;
/// The most input one Send carries, its base64 and envelope within
/// [`MAX_ENVELOPE_BYTES`].
pub(super) const SEND_BYTES: usize = 64 * 1024;

const SHELL: &str = "http://schemas.microsoft.com/wbem/wsman/1/windows/shell";
const TRANSFER: &str = "http://schemas.xmlsoap.org/ws/2004/09/transfer";
const DONE: &str = "http://schemas.microsoft.com/wbem/wsman/1/windows/shell/CommandState/Done";

/// A WS-Management request every listener answers.
pub(super) const IDENTIFY: &str = "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" \
    xmlns:wsmid=\"http://schemas.dmtf.org/wbem/wsman/identity/1/wsmanidentity.xsd\">\
    <s:Header/><s:Body><wsmid:Identify/></s:Body></s:Envelope>";

/// A shell the service created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ShellId(String);

/// A command the service started in a shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommandId(String);

/// A request on the cmd shell resource.
pub(super) enum Request<'a> {
    Create,
    Command {
        shell: &'a ShellId,
        command: &'a str,
    },
    Send {
        shell: &'a ShellId,
        command: &'a CommandId,
        data: &'a [u8],
        end: bool,
    },
    Receive {
        shell: &'a ShellId,
        command: &'a CommandId,
    },
    Terminate {
        shell: &'a ShellId,
        command: &'a CommandId,
    },
    Delete {
        shell: &'a ShellId,
    },
}

impl Request<'_> {
    pub(super) fn action(&self) -> String {
        match self {
            Self::Create => format!("{TRANSFER}/Create"),
            Self::Command { .. } => format!("{SHELL}/Command"),
            Self::Send { .. } => format!("{SHELL}/Send"),
            Self::Receive { .. } => format!("{SHELL}/Receive"),
            Self::Terminate { .. } => format!("{SHELL}/Signal"),
            Self::Delete { .. } => format!("{TRANSFER}/Delete"),
        }
    }

    /// The envelope of this request to the listener at `to`.
    pub(super) fn envelope(&self, to: &str) -> String {
        let (shell, options, body) = match self {
            Self::Create => (
                None,
                options(&[("WINRS_NOPROFILE", "FALSE"), ("WINRS_CODEPAGE", "65001")]),
                format!(
                    "<rsp:Shell><rsp:InputStreams>stdin</rsp:InputStreams>\
                     <rsp:OutputStreams>stdout stderr</rsp:OutputStreams>\
                     <rsp:IdleTimeOut>PT{IDLE_TIMEOUT_SECS}S</rsp:IdleTimeOut></rsp:Shell>"
                ),
            ),
            Self::Command { shell, command } => (
                Some(*shell),
                // Standard input may carry binary data, so it must stay a raw pipe.
                options(&[
                    ("WINRS_CONSOLEMODE_STDIN", "FALSE"),
                    ("WINRS_SKIP_CMD_SHELL", "FALSE"),
                ]),
                format!(
                    "<rsp:CommandLine><rsp:Command>{}</rsp:Command></rsp:CommandLine>",
                    escape(command)
                ),
            ),
            Self::Send {
                shell,
                command,
                data,
                end,
            } => (
                Some(*shell),
                String::new(),
                format!(
                    "<rsp:Send><rsp:Stream Name=\"stdin\" CommandId=\"{}\"{}>{}</rsp:Stream></rsp:Send>",
                    escape(&command.0),
                    if *end { " End=\"true\"" } else { "" },
                    base64::engine::general_purpose::STANDARD.encode(data)
                ),
            ),
            Self::Receive { shell, command } => (
                Some(*shell),
                String::new(),
                format!(
                    "<rsp:Receive><rsp:DesiredStream CommandId=\"{}\">stdout stderr</rsp:DesiredStream></rsp:Receive>",
                    escape(&command.0)
                ),
            ),
            Self::Terminate { shell, command } => (
                Some(*shell),
                String::new(),
                format!(
                    "<rsp:Signal CommandId=\"{}\"><rsp:Code>{SHELL}/signal/terminate</rsp:Code></rsp:Signal>",
                    escape(&command.0)
                ),
            ),
            Self::Delete { shell } => (Some(*shell), String::new(), String::new()),
        };
        let selector = shell.map_or_else(String::new, |shell| {
            format!(
                "<wsman:SelectorSet><wsman:Selector Name=\"ShellId\">{}</wsman:Selector></wsman:SelectorSet>",
                escape(&shell.0)
            )
        });
        format!(
            "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" \
             xmlns:wsa=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
             xmlns:wsman=\"http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd\" \
             xmlns:rsp=\"{SHELL}\"><s:Header>\
             <wsa:To>{to}</wsa:To>\
             <wsman:ResourceURI s:mustUnderstand=\"true\">{SHELL}/cmd</wsman:ResourceURI>\
             <wsa:ReplyTo><wsa:Address s:mustUnderstand=\"true\">http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:Address></wsa:ReplyTo>\
             <wsa:Action s:mustUnderstand=\"true\">{action}</wsa:Action>\
             <wsman:MaxEnvelopeSize s:mustUnderstand=\"true\">{MAX_ENVELOPE_BYTES}</wsman:MaxEnvelopeSize>\
             <wsa:MessageID>uuid:{id}</wsa:MessageID>\
             <wsman:Locale xml:lang=\"en-US\" s:mustUnderstand=\"false\"/>\
             <wsman:OperationTimeout>PT{OPERATION_TIMEOUT_SECS}S</wsman:OperationTimeout>\
             {selector}{options}</s:Header><s:Body>{body}</s:Body></s:Envelope>",
            to = escape(to),
            action = self.action(),
            id = uuid::Uuid::new_v4(),
        )
    }
}

fn options(options: &[(&str, &str)]) -> String {
    let options: String = (options.iter())
        .map(|(name, value)| format!("<wsman:Option Name=\"{name}\">{value}</wsman:Option>"))
        .collect();
    format!("<wsman:OptionSet>{options}</wsman:OptionSet>")
}

fn escape(text: &str) -> String {
    quick_xml::escape::escape(text).into_owned()
}

/// What the service answered.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Response {
    Reply(Reply),
    /// A Receive found no output within its operation timeout.
    TimedOut(Fault),
    Fault(Fault),
}

/// A command's standard output streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stream {
    Stdout,
    Stderr,
}

/// What a successful reply says, as far as the client reads it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Reply {
    pub shell: Option<ShellId>,
    pub command: Option<CommandId>,
    /// Output in the order the service sent it.
    pub output: Vec<(Stream, Vec<u8>)>,
    /// Set once the command has finished, with its exit code.
    pub exit: Option<i32>,
}

impl Reply {
    pub(super) fn shell(self) -> Result<ShellId, DecodeError> {
        self.shell.ok_or(DecodeError::NoShell)
    }

    pub(super) fn command(self) -> Result<CommandId, DecodeError> {
        self.command.ok_or(DecodeError::NoCommand)
    }
}

/// The parts of a reply read so far.
#[derive(Default)]
struct Parsed {
    reply: Reply,
    fault: Option<Fault>,
    stream: Option<Stream>,
    action: Option<String>,
}

impl Response {
    /// The service's answer to `request`: a fault, or a reply naming the action that
    /// answers it.
    pub(super) fn parse(xml: &str, request: &Request<'_>) -> Result<Self, DecodeError> {
        let mut parsed = Parsed::default();
        let mut reader = Reader::from_str(xml);
        let mut path: Vec<String> = Vec::new();
        let mut text = String::new();
        loop {
            match reader.read_event()? {
                Event::Start(element) => {
                    parsed.start(&element)?;
                    path.push(element.local_name().as_ref().to_owned());
                    text.clear();
                }
                Event::Empty(element) => parsed.start(&element)?,
                Event::Text(content) => {
                    text.push_str(&content.xml_content(XmlVersion::Implicit1_0))
                }
                Event::GeneralRef(reference) => text.push(entity(&reference)?),
                Event::End(_) => {
                    let name = path.pop().ok_or(DecodeError::Attribute)?;
                    parsed.end(&name, path.last().map(String::as_str), text.trim())?;
                    text.clear();
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(match parsed.fault {
            Some(fault) if fault.subcode == "TimedOut" => Self::TimedOut(fault),
            Some(fault) => Self::Fault(fault),
            None if parsed.action == Some(format!("{}Response", request.action())) => {
                Self::Reply(parsed.reply)
            }
            None => return Err(DecodeError::Unanswered),
        })
    }
}

impl Parsed {
    fn start(&mut self, element: &BytesStart<'_>) -> Result<(), DecodeError> {
        match element.local_name().as_ref() {
            "Stream" => {
                self.stream = match attribute(element, "Name")?.as_deref() {
                    Some("stdout") => Some(Stream::Stdout),
                    Some("stderr") => Some(Stream::Stderr),
                    _ => return Err(DecodeError::Stream),
                }
            }
            "CommandState" => {
                if attribute(element, "State")?.as_deref() == Some(DONE) {
                    self.reply.exit.get_or_insert(0);
                }
            }
            "Fault" => {
                self.fault = Some(Fault {
                    subcode: String::new(),
                    code: None,
                    reason: String::new(),
                });
            }
            "WSManFault" => {
                let code = attribute(element, "Code")?;
                if let Some(fault) = &mut self.fault {
                    fault.code = code.and_then(|code| code.parse().ok());
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, name: &str, parent: Option<&str>, text: &str) -> Result<(), DecodeError> {
        match (name, parent) {
            ("Action", Some("Header")) => self.action = Some(text.to_owned()),
            ("ShellId", _) => self.reply.shell = Some(ShellId(text.to_owned())),
            ("CommandId", _) => self.reply.command = Some(CommandId(text.to_owned())),
            ("ExitCode", _) => self.reply.exit = Some(exit_code(text)?),
            ("Stream", _) => {
                let bytes = (base64::engine::general_purpose::STANDARD.decode(text))
                    .map_err(|_| DecodeError::Base64)?;
                let stream = self.stream.take().ok_or(DecodeError::Stream)?;
                if !bytes.is_empty() {
                    self.reply.output.push((stream, bytes));
                }
            }
            ("Value", Some("Subcode")) => {
                if let Some(fault) = &mut self.fault {
                    fault.subcode = text.rsplit(':').next().unwrap_or(text).to_owned();
                }
            }
            ("Text", Some("Reason")) => {
                if let Some(fault) = &mut self.fault {
                    fault.reason = text.to_owned();
                }
            }
            ("Message", Some("WSManFault")) => {
                if let Some(fault) = &mut self.fault
                    && fault.reason.is_empty()
                {
                    fault.reason = text.to_owned();
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// An exit code as Windows holds it: 32 bits, which the service may write unsigned.
fn exit_code(text: &str) -> Result<i32, DecodeError> {
    let code: i64 = text.parse().map_err(|_| DecodeError::ExitCode)?;
    (i32::try_from(code).ok())
        .or_else(|| {
            u32::try_from(code)
                .ok()
                .map(|code| i32::from_ne_bytes(code.to_ne_bytes()))
        })
        .ok_or(DecodeError::ExitCode)
}

/// The character an entity or character reference stands for.
fn entity(reference: &BytesRef<'_>) -> Result<char, DecodeError> {
    if let Some(character) = reference.resolve_char_ref()? {
        return Ok(character);
    }
    match &**reference {
        "amp" => Ok('&'),
        "lt" => Ok('<'),
        "gt" => Ok('>'),
        "quot" => Ok('"'),
        "apos" => Ok('\''),
        _ => Err(DecodeError::Attribute),
    }
}

fn attribute(element: &BytesStart<'_>, name: &str) -> Result<Option<String>, DecodeError> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| DecodeError::Attribute)?;
        if attribute.key.local_name().as_ref() == name {
            let value = (attribute.normalized_value(XmlVersion::Implicit1_0))
                .map_err(|_| DecodeError::Attribute)?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
pub(super) mod tests {
    use std::assert_matches;

    use super::*;

    /// Requests name their shell and command, escape text, and carry input as base64
    /// with its end marked.
    #[test]
    fn requests_name_their_shell_and_escape_text() {
        let to = "https://host:5986/wsman";
        let shell = ShellId("S1".into());
        let command = CommandId("C1".into());
        let started = Request::Command {
            shell: &shell,
            command: "cd /d \"%USERPROFILE%\" && a<b",
        }
        .envelope(to);
        for expected in [
            "<wsman:Selector Name=\"ShellId\">S1</wsman:Selector>",
            "<rsp:Command>cd /d &quot;%USERPROFILE%&quot; &amp;&amp; a&lt;b</rsp:Command>",
            "<wsman:Option Name=\"WINRS_CONSOLEMODE_STDIN\">FALSE</wsman:Option>",
            "/windows/shell/Command</wsa:Action>",
        ] {
            assert!(started.contains(expected), "{expected}: {started}");
        }
        let send = Request::Send {
            shell: &shell,
            command: &command,
            data: b"hi",
            end: true,
        }
        .envelope(to);
        assert!(send.contains(
            "<rsp:Stream Name=\"stdin\" CommandId=\"C1\" End=\"true\">aGk=</rsp:Stream>"
        ));
        assert!(!Request::Create.envelope(to).contains("SelectorSet"));
    }

    /// An envelope answering `action` with `body`.
    pub(in super::super) fn answer(action: &str, body: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="s" xmlns:a="a" xmlns:rsp="r"><s:Header><a:Action>{action}Response</a:Action></s:Header><s:Body>{body}</s:Body></s:Envelope>"#
        )
    }

    /// Replies yield their identifiers, output in order, exit codes as Windows holds
    /// them, and faults, a Receive's timeout apart from the rest. A reply that names
    /// no action answering the request, as an empty one, is malformed.
    #[test]
    fn replies_yield_identifiers_output_exit_codes_and_faults() {
        let (shell, command) = (ShellId("S".into()), CommandId("C".into()));
        let receive = Request::Receive {
            shell: &shell,
            command: &command,
        };
        let reply = |xml: &str, request: &Request<'_>| match Response::parse(xml, request) {
            Ok(Response::Reply(reply)) => reply,
            other => panic!("{other:?}"),
        };
        let created = answer(
            &Request::Create.action(),
            "<rsp:Shell><rsp:ShellId>ABC</rsp:ShellId></rsp:Shell>",
        );
        let created = reply(&created, &Request::Create);
        assert_eq!(created.shell().unwrap(), ShellId("ABC".into()));
        let received = answer(
            &receive.action(),
            &format!(
                r#"<rsp:ReceiveResponse>
            <rsp:Stream Name="stdout" CommandId="C">aGVs</rsp:Stream>
            <rsp:Stream Name="stderr" CommandId="C">ZXJy</rsp:Stream>
            <rsp:Stream Name="stdout" CommandId="C">bG8=</rsp:Stream>
            <rsp:Stream Name="stdout" CommandId="C" End="true"></rsp:Stream>
            <rsp:CommandState CommandId="C" State="{DONE}"><rsp:ExitCode>3221225786</rsp:ExitCode></rsp:CommandState>
            </rsp:ReceiveResponse>"#
            ),
        );
        let received = reply(&received, &receive);
        let output = [
            (Stream::Stdout, b"hel".to_vec()),
            (Stream::Stderr, b"err".to_vec()),
            (Stream::Stdout, b"lo".to_vec()),
        ];
        assert_eq!(received.output, output);
        assert_eq!(received.exit, Some(0xC000_013A_u32 as i32));
        let running = answer(
            &receive.action(),
            r#"<rsp:ReceiveResponse><rsp:CommandState CommandId="C" State="http://schemas.microsoft.com/wbem/wsman/1/windows/shell/CommandState/Running"/></rsp:ReceiveResponse>"#,
        );
        assert_eq!(reply(&running, &receive).exit, None);
        let delete = Request::Delete { shell: &shell };
        for unanswered in ["", "<html><body>Bad Gateway</body></html>", &running] {
            assert_matches!(
                Response::parse(unanswered, &delete),
                Err(DecodeError::Unanswered)
            );
        }

        let fault = |subcode: &str| {
            format!(
                r#"<s:Envelope xmlns:s="s" xmlns:w="w" xmlns:f="f"><s:Body><s:Fault>
                <s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>w:{subcode}</s:Value></s:Subcode></s:Code>
                <s:Reason><s:Text xml:lang="">Gone &amp; done.</s:Text></s:Reason>
                <s:Detail><f:WSManFault Code="2150858843" Machine="m"><f:Message>m</f:Message></f:WSManFault></s:Detail>
                </s:Fault></s:Body></s:Envelope>"#
            )
        };
        let expected = Fault {
            subcode: "InvalidSelectors".into(),
            code: Some(2_150_858_843),
            reason: "Gone & done.".into(),
        };
        assert_eq!(
            Response::parse(&fault("InvalidSelectors"), &delete).unwrap(),
            Response::Fault(expected.clone())
        );
        assert_matches!(
            Response::parse(&fault("TimedOut"), &receive),
            Ok(Response::TimedOut(_))
        );

        assert!(Response::parse("<a><b></a>", &receive).is_err());
        let bad_stream = r#"<rsp:Stream xmlns:rsp="r" Name="stdout">!!</rsp:Stream>"#;
        assert_matches!(
            Response::parse(bad_stream, &receive),
            Err(DecodeError::Base64)
        );
    }
}
