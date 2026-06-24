use std::io;
use std::path::{Path, PathBuf};

pub use tonic_prost_build::FileDescriptorSet;

#[derive(Debug, Clone, Default)]
pub struct CompileOptions {
    pub out_dir: Option<PathBuf>,
    pub descriptor_path: Option<PathBuf>,
}

pub fn compile_protos<P>(protos: &[P], includes: &[P]) -> io::Result<()>
where
    P: AsRef<Path>,
{
    compile_protos_with_options(protos, includes, &CompileOptions::default())
}

pub fn compile_protos_with_options<P>(
    protos: &[P],
    includes: &[P],
    options: &CompileOptions,
) -> io::Result<()>
where
    P: AsRef<Path>,
{
    let protoc = protoc_bin_vendored::protoc_bin_path().map_err(vendored_error)?;
    let vendored_include = protoc_bin_vendored::include_path().map_err(vendored_error)?;
    let proto_paths = protos
        .iter()
        .map(|path| path.as_ref().to_path_buf())
        .collect::<Vec<_>>();
    let mut include_paths = includes
        .iter()
        .map(|path| path.as_ref().to_path_buf())
        .collect::<Vec<_>>();
    if !include_paths.contains(&vendored_include) {
        include_paths.push(vendored_include);
    }

    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc);
    let mut builder = tonic_prost_build::configure();
    if let Some(path) = &options.out_dir {
        builder = builder.out_dir(path);
    }
    if let Some(path) = &options.descriptor_path {
        builder = builder.file_descriptor_set_path(path);
    }
    builder.compile_with_config(prost, &proto_paths, &include_paths)
}

pub fn vendored_protoc_path() -> io::Result<PathBuf> {
    protoc_bin_vendored::protoc_bin_path().map_err(vendored_error)
}

pub fn vendored_include_path() -> io::Result<PathBuf> {
    protoc_bin_vendored::include_path().map_err(vendored_error)
}

fn vendored_error(error: protoc_bin_vendored::Error) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_vendored_protoc() {
        assert!(vendored_protoc_path().unwrap().is_file());
        assert!(vendored_include_path().unwrap().is_dir());
    }

    #[test]
    fn compiles_proto_and_descriptor() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("base-rpc-build-{}-{unique}", std::process::id()));
        let proto_dir = root.join("proto");
        let out_dir = root.join("out");
        std::fs::create_dir_all(&proto_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();
        let proto = proto_dir.join("echo.proto");
        std::fs::write(
            &proto,
            "syntax = \"proto3\"; package test.echo.v1; message Ping { string value = 1; } service Echo { rpc Call(Ping) returns (Ping); }",
        )
        .unwrap();
        let descriptor = out_dir.join("descriptor.bin");
        compile_protos_with_options(
            &[proto],
            &[proto_dir],
            &CompileOptions {
                out_dir: Some(out_dir.clone()),
                descriptor_path: Some(descriptor.clone()),
            },
        )
        .unwrap();
        assert!(out_dir.join("test.echo.v1.rs").is_file());
        assert!(descriptor.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
}
