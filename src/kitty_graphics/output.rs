//! Ordered, local-only graphics output. Upload pixels stay raw until output I/O.
#[cfg(test)]
use std::io::{self, Write};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(crate) enum GraphicsOperation {
    Bytes(Vec<u8>),
    Upload { control: String, data: Arc<[u8]> },
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GraphicsOutput {
    pub(crate) operations: Vec<GraphicsOperation>,
    /// The client must repaint every cell on the next frame. Sixel output has no
    /// erase sequence, so dropping a painted frame can only be fixed by writing
    /// the covered cells again.
    pub(crate) repaint_required: bool,
}

impl GraphicsOutput {
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        let mut output = Self::default();
        output.push_bytes(bytes);
        output
    }

    pub(crate) fn from_ops(operations: Vec<GraphicsOperation>) -> Self {
        Self {
            operations,
            ..Self::default()
        }
    }

    /// Marks that the outer terminal needs a full repaint to clear painted frames.
    pub(crate) fn require_repaint(&mut self) {
        self.repaint_required = true;
    }

    pub(crate) fn push_bytes(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.operations.push(GraphicsOperation::Bytes(bytes));
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Diagnostics-only: serialize every operation exactly as the client writes
    /// it, including Kitty uploads, so a debug run can export the complete host
    /// byte stream instead of a truncated prefix.
    pub(crate) fn diagnostics_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for operation in &self.operations {
            match operation {
                GraphicsOperation::Bytes(next) => bytes.extend_from_slice(next),
                GraphicsOperation::Upload { control, data } => {
                    super::encode_kitty_data(&mut bytes, control, data);
                }
            }
        }
        bytes
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.operations.extend(other.operations);
    }

    #[cfg(test)]
    pub(crate) fn into_inline_bytes(self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for operation in self.operations {
            match operation {
                GraphicsOperation::Bytes(next) if bytes.is_empty() => bytes = next,
                GraphicsOperation::Bytes(next) => bytes.extend(next),
                GraphicsOperation::Upload { control, data } => {
                    super::encode_kitty_data(&mut bytes, &control, &data);
                }
            }
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_fallback_preserves_order_and_chunk_encoding() {
        let data: Arc<[u8]> = Arc::from(vec![42; super::super::KITTY_CHUNK_BYTES + 1]);
        let control = "a=t,t=d,f=24,s=1,v=1,i=7,q=2";
        let mut output = GraphicsOutput::default();
        assert!(output.is_empty());
        output.extend(GraphicsOutput::from_bytes(b"before".to_vec()));
        output.operations.push(GraphicsOperation::Upload {
            control: control.into(),
            data: Arc::clone(&data),
        });
        output.extend(GraphicsOutput::from_bytes(b"after".to_vec()));
        let mut expected = b"before".to_vec();
        super::super::encode_kitty_data(&mut expected, control, &data);
        expected.extend_from_slice(b"after");
        assert_eq!(output.into_inline_bytes(), expected);
    }

    #[test]
    fn diagnostics_bytes_serializes_the_complete_output() {
        let data: Arc<[u8]> = Arc::from(vec![7; super::super::KITTY_CHUNK_BYTES + 1]);
        let control = "a=t,t=d,f=24,s=1,v=1,i=9,q=2";
        let mut output = GraphicsOutput::default();
        output.extend(GraphicsOutput::from_bytes(vec![b'x'; 4096]));
        output.operations.push(GraphicsOperation::Upload {
            control: control.into(),
            data: Arc::clone(&data),
        });
        output.extend(GraphicsOutput::from_bytes(b"tail".to_vec()));
        let mut expected = vec![b'x'; 4096];
        super::super::encode_kitty_data(&mut expected, control, &data);
        expected.extend_from_slice(b"tail");
        let dumped = output.diagnostics_bytes();
        assert_eq!(dumped, expected);
        assert!(
            dumped.len() > 220,
            "diagnostics must not truncate the painted stream"
        );
    }

    #[test]
    fn inline_write_propagates_io_errors() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            super::super::write_kitty_data(&mut Broken, "a=t,t=d,f=32", &[1, 2, 3, 4])
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
