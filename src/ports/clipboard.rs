use std::path::PathBuf;

use color_eyre::eyre::Result;

/// Semantic access to local clipboard text and file references.
///
/// Platform implementations may block while invoking OS services; async
/// orchestration must run these calls on controlled blocking workers.
pub trait Clipboard: Send + Sync {
    fn read_text(&self) -> Result<Option<String>>;
    fn write_text(&self, text: &str) -> Result<()>;
    fn read_files(&self) -> Result<Vec<PathBuf>>;
    fn write_files(&self, paths: &[PathBuf]) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EmptyClipboard;

    impl Clipboard for EmptyClipboard {
        fn read_text(&self) -> Result<Option<String>> {
            Ok(None)
        }

        fn write_text(&self, _text: &str) -> Result<()> {
            Ok(())
        }

        fn read_files(&self) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }

        fn write_files(&self, _paths: &[PathBuf]) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn clipboard_port_is_object_safe_and_models_absent_content() {
        fn assert_object_safe(_: &dyn Clipboard) {}
        let clipboard = EmptyClipboard;

        assert_object_safe(&clipboard);
        assert_eq!(clipboard.read_text().unwrap(), None);
        assert!(clipboard.read_files().unwrap().is_empty());
    }
}
