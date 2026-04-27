use anyhow::{Context as _, Result, anyhow};
use std::io::{Read, Write};
use std::mem::size_of;

#[cfg(any(test, target_os = "ios"))]
use crate::remote_client::{CommandTemplate, Interactive};
#[cfg(any(test, target_os = "ios"))]
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RusshWindowSize {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RusshHelperRequest {
    pub command: String,
    pub interactive: bool,
    pub initial_window_size: Option<RusshWindowSize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RusshHelperFrame {
    Stdin(Vec<u8>),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Resize(RusshWindowSize),
    Eof,
    ExitStatus(i32),
    Error(String),
}

const TAG_STDIN: u8 = 0;
const TAG_STDOUT: u8 = 1;
const TAG_STDERR: u8 = 2;
const TAG_RESIZE: u8 = 3;
const TAG_EOF: u8 = 4;
const TAG_EXIT_STATUS: u8 = 5;
const TAG_ERROR: u8 = 6;
const LENGTH_PREFIX_SIZE: usize = size_of::<u32>();

impl RusshHelperRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        write_string(&mut payload, &self.command)?;
        payload.push(u8::from(self.interactive));
        match self.initial_window_size {
            Some(size) => {
                payload.push(1);
                write_window_size(&mut payload, size);
            }
            None => {
                payload.push(0);
            }
        }
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let command = decoder.read_string()?;
        let interactive = decoder.read_bool()?;
        let initial_window_size = if decoder.read_bool()? {
            Some(decoder.read_window_size()?)
        } else {
            None
        };
        decoder.finish()?;
        Ok(Self {
            command,
            interactive,
            initial_window_size,
        })
    }

    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<()> {
        write_length_prefixed_payload(writer, &self.encode()?)
    }

    pub fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        Self::decode(&read_length_prefixed_payload(reader)?)
    }
}

impl RusshHelperFrame {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        match self {
            Self::Stdin(data) => {
                payload.push(TAG_STDIN);
                write_bytes(&mut payload, data)?;
            }
            Self::Stdout(data) => {
                payload.push(TAG_STDOUT);
                write_bytes(&mut payload, data)?;
            }
            Self::Stderr(data) => {
                payload.push(TAG_STDERR);
                write_bytes(&mut payload, data)?;
            }
            Self::Resize(size) => {
                payload.push(TAG_RESIZE);
                write_window_size(&mut payload, *size);
            }
            Self::Eof => {
                payload.push(TAG_EOF);
            }
            Self::ExitStatus(status) => {
                payload.push(TAG_EXIT_STATUS);
                payload.extend_from_slice(&status.to_le_bytes());
            }
            Self::Error(message) => {
                payload.push(TAG_ERROR);
                write_string(&mut payload, message)?;
            }
        }
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let tag = decoder.read_u8()?;
        let frame = match tag {
            TAG_STDIN => Self::Stdin(decoder.read_bytes()?),
            TAG_STDOUT => Self::Stdout(decoder.read_bytes()?),
            TAG_STDERR => Self::Stderr(decoder.read_bytes()?),
            TAG_RESIZE => Self::Resize(decoder.read_window_size()?),
            TAG_EOF => Self::Eof,
            TAG_EXIT_STATUS => Self::ExitStatus(decoder.read_i32()?),
            TAG_ERROR => Self::Error(decoder.read_string()?),
            _ => {
                anyhow::bail!("Unknown Russh helper frame tag: {tag}");
            }
        };
        decoder.finish()?;
        Ok(frame)
    }

    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<()> {
        write_length_prefixed_payload(writer, &self.encode()?)
    }

    pub fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        Self::decode(&read_length_prefixed_payload(reader)?)
    }
}

#[cfg(any(test, target_os = "ios"))]
pub fn build_russh_helper_command_template(
    executable_path: &Path,
    socket_path: &Path,
    command: String,
    interactive: Interactive,
) -> CommandTemplate {
    let interactive = match interactive {
        Interactive::Yes => "yes",
        Interactive::No => "no",
    };

    CommandTemplate {
        program: executable_path.display().to_string(),
        args: vec![
            "--zed-ios-russh-helper".into(),
            "exec".into(),
            "--socket".into(),
            socket_path.display().to_string(),
            "--interactive".into(),
            interactive.into(),
            "--command".into(),
            command,
        ],
        env: Default::default(),
    }
}

fn write_length_prefixed_payload<W: Write>(writer: &mut W, payload: &[u8]) -> Result<()> {
    let length = u32::try_from(payload.len()).context("Russh helper payload exceeds u32")?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn read_length_prefixed_payload<R: Read>(reader: &mut R) -> Result<Vec<u8>> {
    let mut length_buffer = [0; LENGTH_PREFIX_SIZE];
    reader.read_exact(&mut length_buffer)?;
    let payload_length = u32::from_le_bytes(length_buffer) as usize;
    let mut payload = vec![0; payload_length];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

fn write_string(payload: &mut Vec<u8>, value: &str) -> Result<()> {
    write_bytes(payload, value.as_bytes())
}

fn write_bytes(payload: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    let length = u32::try_from(value.len()).context("Russh helper field exceeds u32")?;
    payload.extend_from_slice(&length.to_le_bytes());
    payload.extend_from_slice(value);
    Ok(())
}

fn write_window_size(payload: &mut Vec<u8>, size: RusshWindowSize) {
    payload.extend_from_slice(&size.columns.to_le_bytes());
    payload.extend_from_slice(&size.rows.to_le_bytes());
    payload.extend_from_slice(&size.pixel_width.to_le_bytes());
    payload.extend_from_slice(&size.pixel_height.to_le_bytes());
}

struct Decoder<'a> {
    payload: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self { payload, offset: 0 }
    }

    fn read_u8(&mut self) -> Result<u8> {
        let value = self
            .payload
            .get(self.offset)
            .copied()
            .ok_or_else(|| anyhow!("Unexpected end of Russh helper payload"))?;
        self.offset += 1;
        Ok(value)
    }

    fn read_bool(&mut self) -> Result<bool> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => anyhow::bail!("Invalid Russh helper bool value: {other}"),
        }
    }

    fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read_fixed()?))
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_fixed()?))
    }

    fn read_i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.read_fixed()?))
    }

    fn read_fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.offset + N;
        let bytes = self
            .payload
            .get(self.offset..end)
            .ok_or_else(|| anyhow!("Unexpected end of Russh helper payload"))?;
        self.offset = end;
        bytes
            .try_into()
            .map_err(|_| anyhow!("Failed to decode Russh helper payload"))
    }

    fn read_bytes(&mut self) -> Result<Vec<u8>> {
        let length = self.read_u32()? as usize;
        let end = self.offset + length;
        let bytes = self
            .payload
            .get(self.offset..end)
            .ok_or_else(|| anyhow!("Unexpected end of Russh helper payload"))?;
        self.offset = end;
        Ok(bytes.to_vec())
    }

    fn read_string(&mut self) -> Result<String> {
        String::from_utf8(self.read_bytes()?).context("Russh helper payload contained invalid UTF-8")
    }

    fn read_window_size(&mut self) -> Result<RusshWindowSize> {
        Ok(RusshWindowSize {
            columns: self.read_u16()?,
            rows: self.read_u16()?,
            pixel_width: self.read_u16()?,
            pixel_height: self.read_u16()?,
        })
    }

    fn finish(self) -> Result<()> {
        if self.offset == self.payload.len() {
            Ok(())
        } else {
            anyhow::bail!(
                "Russh helper payload had {} trailing bytes",
                self.payload.len() - self.offset
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RusshHelperFrame, RusshHelperRequest, RusshWindowSize, build_russh_helper_command_template,
    };
    use crate::Interactive;
    use std::path::Path;

    #[test]
    fn request_round_trip_serialization() {
        let request = RusshHelperRequest {
            command: "cd \"$HOME\" && exec env TERM=xterm bash -l".into(),
            interactive: true,
            initial_window_size: Some(RusshWindowSize {
                columns: 120,
                rows: 40,
                pixel_width: 1440,
                pixel_height: 900,
            }),
        };

        let mut buffer = Vec::new();
        request.write_to(&mut buffer).unwrap();
        let decoded = RusshHelperRequest::read_from(&mut buffer.as_slice()).unwrap();

        assert_eq!(decoded, request);
    }

    #[test]
    fn interactive_and_non_interactive_requests_encode_differently() {
        let command = "exec env printf hello".to_string();
        let interactive_request = RusshHelperRequest {
            command: command.clone(),
            interactive: true,
            initial_window_size: Some(RusshWindowSize {
                columns: 80,
                rows: 24,
                pixel_width: 0,
                pixel_height: 0,
            }),
        };
        let non_interactive_request = RusshHelperRequest {
            command,
            interactive: false,
            initial_window_size: None,
        };

        let interactive_bytes = interactive_request.encode().unwrap();
        let non_interactive_bytes = non_interactive_request.encode().unwrap();

        assert_ne!(interactive_bytes, non_interactive_bytes);
        assert!(RusshHelperRequest::decode(&interactive_bytes)
            .unwrap()
            .interactive);
        assert!(!RusshHelperRequest::decode(&non_interactive_bytes)
            .unwrap()
            .interactive);
    }

    #[test]
    fn resize_frame_round_trip() {
        let frame = RusshHelperFrame::Resize(RusshWindowSize {
            columns: 132,
            rows: 43,
            pixel_width: 1584,
            pixel_height: 972,
        });

        let mut buffer = Vec::new();
        frame.write_to(&mut buffer).unwrap();
        let decoded = RusshHelperFrame::read_from(&mut buffer.as_slice()).unwrap();

        assert_eq!(decoded, frame);
    }

    #[test]
    fn frame_round_trip_for_stdio_and_exit_status() {
        let frames = vec![
            RusshHelperFrame::Stdin(vec![1, 2, 3]),
            RusshHelperFrame::Stdout(vec![4, 5, 6]),
            RusshHelperFrame::Stderr(vec![7, 8, 9]),
            RusshHelperFrame::Eof,
            RusshHelperFrame::ExitStatus(17),
            RusshHelperFrame::Error("boom".into()),
        ];

        for frame in frames {
            let encoded = frame.encode().unwrap();
            let decoded = RusshHelperFrame::decode(&encoded).unwrap();
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn helper_command_template_uses_expected_argv_format() {
        let command = build_russh_helper_command_template(
            Path::new("/Applications/Zed.app/Zed"),
            Path::new("/tmp/zed-russh/helper.sock"),
            "cd \"$HOME/project\" && exec env FOO=bar bash -l".into(),
            Interactive::Yes,
        );

        assert_eq!(command.program, "/Applications/Zed.app/Zed");
        assert_eq!(
            command.args,
            vec![
                "--zed-ios-russh-helper",
                "exec",
                "--socket",
                "/tmp/zed-russh/helper.sock",
                "--interactive",
                "yes",
                "--command",
                "cd \"$HOME/project\" && exec env FOO=bar bash -l",
            ]
        );
        assert!(command.env.is_empty());
    }

    #[test]
    fn helper_command_template_marks_non_interactive_commands() {
        let command = build_russh_helper_command_template(
            Path::new("/Applications/Zed.app/Zed"),
            Path::new("/tmp/zed-russh/helper.sock"),
            "exec env printf hello".into(),
            Interactive::No,
        );

        assert_eq!(command.args[5], "no");
    }
}
