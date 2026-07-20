use std::path::PathBuf;

use color_eyre::eyre::Result;

use crate::ports::Clipboard;

/// Production clipboard adapter for Linux and macOS command-backed access.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformClipboard;

impl Clipboard for PlatformClipboard {
    fn read_text(&self) -> Result<Option<String>> {
        super::sync::read_clipboard().map(Some)
    }

    fn write_text(&self, text: &str) -> Result<()> {
        super::sync::write_clipboard(text)
    }

    fn read_files(&self) -> Result<Vec<PathBuf>> {
        Ok(crate::filetransfer::clipboard_files::get_clipboard_files().unwrap_or_default())
    }

    fn write_files(&self, paths: &[PathBuf]) -> Result<()> {
        crate::filetransfer::clipboard_files::set_clipboard_files(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_adapter_implements_the_semantic_clipboard_boundary() {
        fn assert_clipboard(_: &dyn Clipboard) {}
        assert_clipboard(&PlatformClipboard);
    }
}
