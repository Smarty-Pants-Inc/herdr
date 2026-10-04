use std::collections::HashSet;
use std::io::{self, Write as _};
use std::sync::{Mutex, OnceLock};

use crate::kitty_graphics::{GraphicsOperation, GraphicsOutput};
use crate::protocol::{render_ansi, FrameData};
use base64::Engine as _;

/// Local output only; raw uploads are never added to a published frame codec.
#[derive(Debug)]
pub(crate) struct ComposedFrame {
    pub(crate) frame: FrameData,
    pub(crate) graphics: GraphicsOutput,
}

impl From<FrameData> for ComposedFrame {
    fn from(mut frame: FrameData) -> Self {
        let graphics = GraphicsOutput::from_bytes(std::mem::take(&mut frame.graphics));
        Self { frame, graphics }
    }
}

impl std::ops::Deref for ComposedFrame {
    type Target = FrameData;

    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}

static RECEIVED_KITTY_GRAPHICS_IDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

pub(super) fn write_composed_frame(
    mut writer: impl io::Write,
    encoded: &[u8],
    graphics: &GraphicsOutput,
    files: &mut super::image_files::FileTransport,
) -> io::Result<()> {
    let mut pending_ledger = Vec::new();
    if graphics.is_empty() {
        return writer.write_all(encoded);
    }
    let mut writer = io::BufWriter::with_capacity(64 * 1024, writer);
    let insertion = render_ansi::final_sync_output_end(encoded).unwrap_or(encoded.len());
    writer.write_all(&encoded[..insertion])?;
    writer.write_all(b"\x1b7")?;
    for operation in &graphics.operations {
        match operation {
            GraphicsOperation::Bytes(bytes) => {
                let commands = kitty_graphics_image_commands(bytes);
                apply_upload_ledger_obligations(&commands);
                writer.write_all(bytes)?;
                pending_ledger.extend(commands);
            }
            GraphicsOperation::Upload { control, data } => {
                let header = format!("\x1b_G{control};\x1b\\");
                let header_commands = kitty_graphics_image_commands(header.as_bytes());
                apply_upload_ledger_obligations(&header_commands);
                let file_eligible = control
                    .split(',')
                    .any(|part| matches!(part, "f=24" | "f=32" | "f=100"));
                if file_eligible {
                    if let Some(path) = files
                        .probe()
                        .and_then(|path| path.to_str().map(str::to_owned))
                    {
                        let path =
                            base64::engine::general_purpose::STANDARD.encode(path.as_bytes());
                        write!(writer, "\x1b_Ga=q,t=t,f=32,s=1,v=1,i=1,q=2;{path}\x1b\\")?;
                    }
                }
                let path = file_eligible.then(|| files.prepare(data)).flatten();
                if let Some(path) = path.as_ref().and_then(|path| path.to_str()) {
                    let control = control.replace(",t=d,", ",t=t,");
                    let path = base64::engine::general_purpose::STANDARD.encode(path.as_bytes());
                    write!(writer, "\x1b_G{control};{path}\x1b\\")?;
                } else {
                    crate::kitty_graphics::write_kitty_data(&mut writer, control, data)?;
                }
                pending_ledger.extend(kitty_graphics_image_commands(header.as_bytes()));
            }
        }
    }
    writer.write_all(b"\x1b8")?;
    if let Err(error) = writer.write_all(&encoded[insertion..]) {
        apply_upload_ledger_obligations(&pending_ledger);
        return Err(error);
    }
    if let Err(error) = io::Write::flush(&mut writer) {
        apply_upload_ledger_obligations(&pending_ledger);
        return Err(error);
    }
    apply_graphics_ledger_changes(pending_ledger);
    Ok(())
}

pub(super) fn write_encoded_frame_with_graphics(
    mut writer: impl io::Write,
    encoded: &[u8],
    graphics: &[u8],
) -> io::Result<()> {
    if graphics.is_empty() {
        return writer.write_all(encoded);
    }

    let insertion = render_ansi::final_sync_output_end(encoded).unwrap_or(encoded.len());
    apply_upload_ledger_obligations(&kitty_graphics_image_commands(graphics));

    writer.write_all(&encoded[..insertion])?;
    writer.write_all(b"\x1b7")?;
    writer.write_all(graphics)?;
    writer.write_all(b"\x1b8")?;
    if let Err(error) = writer.write_all(&encoded[insertion..]) {
        apply_upload_ledger_obligations(&kitty_graphics_image_commands(graphics));
        return Err(error);
    }
    if let Err(error) = writer.flush() {
        apply_upload_ledger_obligations(&kitty_graphics_image_commands(graphics));
        return Err(error);
    }
    record_received_kitty_graphics(graphics);
    Ok(())
}

pub(super) fn contains_kitty_graphics_bytes(bytes: &[u8]) -> bool {
    bytes.windows(3).any(|window| window == b"\x1b_G")
}

pub(super) fn record_received_kitty_graphics(bytes: &[u8]) {
    let commands = kitty_graphics_image_commands(bytes);
    if commands.is_empty() {
        return;
    }
    apply_graphics_ledger_changes(commands);
}

fn apply_upload_ledger_obligations(commands: &[KittyGraphicsImageCommand]) {
    let uploads = commands
        .iter()
        .filter_map(|command| match command {
            KittyGraphicsImageCommand::Upload(id) => Some(KittyGraphicsImageCommand::Upload(*id)),
            KittyGraphicsImageCommand::Delete(_) => None,
        })
        .collect();
    apply_graphics_ledger_changes(uploads);
}

fn apply_graphics_ledger_changes(commands: Vec<KittyGraphicsImageCommand>) {
    let set = RECEIVED_KITTY_GRAPHICS_IDS.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut set) = set.lock() {
        for command in commands {
            match command {
                KittyGraphicsImageCommand::Upload(id) => {
                    set.insert(id);
                }
                KittyGraphicsImageCommand::Delete(id) => {
                    set.remove(&id);
                }
            }
        }
    }
}

pub(super) fn clear_received_kitty_graphics(mut writer: impl io::Write) -> io::Result<()> {
    let Some(set) = RECEIVED_KITTY_GRAPHICS_IDS.get() else {
        return Ok(());
    };
    let Ok(mut set) = set.lock() else {
        return Ok(());
    };
    let mut ids = set.iter().copied().collect::<Vec<_>>();
    ids.sort_unstable();
    for id in &ids {
        write!(writer, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\")?;
    }
    writer.flush()?;
    for id in ids {
        set.remove(&id);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum KittyGraphicsImageCommand {
    Upload(u32),
    Delete(u32),
}

fn kitty_graphics_image_commands(bytes: &[u8]) -> Vec<KittyGraphicsImageCommand> {
    let mut commands = Vec::new();
    let mut index = 0usize;
    while let Some(start) = find_subslice(&bytes[index..], b"\x1b_G") {
        let command_start = index + start + 3;
        let Some(end) = find_subslice(&bytes[command_start..], b"\x1b\\") else {
            break;
        };
        let command = &bytes[command_start..command_start + end];
        if let Some(id) = kitty_graphics_command_image_id(command) {
            let header_end = command
                .iter()
                .position(|byte| *byte == b';')
                .unwrap_or(command.len());
            let mut action = None;
            let mut delete_whole = false;
            for part in command[..header_end].split(|byte| *byte == b',') {
                if let Some(value) = part.strip_prefix(b"a=") {
                    action = Some(value);
                } else if part == b"d=I" {
                    delete_whole = true;
                }
            }
            if delete_whole && action == Some(b"d") {
                commands.push(KittyGraphicsImageCommand::Delete(id));
            } else if action == Some(b"t") {
                commands.push(KittyGraphicsImageCommand::Upload(id));
            }
        }
        index = command_start + end + 2;
    }
    commands
}

#[cfg(test)]
pub(super) fn kitty_graphics_image_ids(bytes: &[u8]) -> Vec<u32> {
    let mut ids = Vec::new();
    let mut index = 0usize;
    while let Some(start) = find_subslice(&bytes[index..], b"\x1b_G") {
        let command_start = index + start + 3;
        let Some(end) = find_subslice(&bytes[command_start..], b"\x1b\\") else {
            break;
        };
        let command = &bytes[command_start..command_start + end];
        if let Some(id) = kitty_graphics_command_image_id(command) {
            ids.push(id);
        }
        index = command_start + end + 2;
    }
    ids
}

fn kitty_graphics_command_image_id(command: &[u8]) -> Option<u32> {
    let header_end = command
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(command.len());
    for part in command[..header_end].split(|byte| *byte == b',') {
        let Some(value) = part.strip_prefix(b"i=") else {
            continue;
        };
        let text = std::str::from_utf8(value).ok()?;
        if let Ok(id) = text.parse::<u32>() {
            return Some(id);
        }
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
