//! Per-instance in-memory filesystem: wk implements `wasi:filesystem` itself
//! (instead of wasmtime-wasi's cap-std one) so each plugin instance sees its own
//! sandboxed, in-RAM filesystem. Nothing touches the host disk.
//!
//! NOTE: methods are currently stubs (return `unsupported`); the in-memory tree
//! and real behaviour land next. This step proves the wiring: our filesystem
//! replaces wasmtime-wasi's and existing plugins still instantiate.

use wasmtime::component::{HasData, Linker, Resource, ResourceTable};
use wasmtime::Result;
use wasmtime_wasi::WasiView;
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream};
use wasmtime_wasi_io::IoView;

wasmtime::component::bindgen!({
    path: "wit-fs",
    world: "fs-host",
    imports: { default: trappable },
    require_store_data_send: true,
    with: {
        // Our files' read/write streams ARE wasmtime-wasi's io streams, so the
        // guest's wasi:io/streams (provided by wasmtime-wasi) can read them.
        "wasi:io/error": wasmtime_wasi_io::bindings::wasi::io::error,
        "wasi:io/poll": wasmtime_wasi_io::bindings::wasi::io::poll,
        "wasi:io/streams": wasmtime_wasi_io::bindings::wasi::io::streams,
        "wasi:filesystem/types.descriptor": Descriptor,
        "wasi:filesystem/types.directory-entry-stream": DirEntryStream,
    },
});

use crate::plugin::HostState;
use wasi::filesystem::types::{
    Advice, DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, Filesize,
    MetadataHashValue, NewTimestamp, OpenFlags, PathFlags,
};

/// A filesystem descriptor handle (open file or directory).
pub struct Descriptor;

/// An in-progress directory listing.
pub struct DirEntryStream;

/// Add every wasmtime-wasi interface our guests use *except* its (cap-std)
/// filesystem, so we can provide our own in-memory filesystem instead.
pub fn add_wasi_except_fs<T: WasiView + 'static>(l: &mut Linker<T>) -> Result<()> {
    use wasmtime_wasi::cli::{WasiCli, WasiCliView};
    use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
    use wasmtime_wasi::p2::bindings::{cli, clocks};

    struct HasIo;
    impl HasData for HasIo {
        type Data<'a> = &'a mut ResourceTable;
    }

    wasmtime_wasi_io::bindings::wasi::io::error::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    wasmtime_wasi_io::bindings::wasi::io::poll::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    wasmtime_wasi_io::bindings::wasi::io::streams::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;

    clocks::wall_clock::add_to_linker::<T, WasiClocks>(l, T::clocks)?;
    clocks::monotonic_clock::add_to_linker::<T, WasiClocks>(l, T::clocks)?;

    cli::exit::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::environment::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::stdin::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::stdout::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::stderr::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_input::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_output::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_stdin::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_stdout::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_stderr::add_to_linker::<T, WasiCli>(l, T::cli)?;
    Ok(())
}

/// Add our in-memory `wasi:filesystem` to the linker.
pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wasi::filesystem::types::add_to_linker::<_, HasFs>(l, |s| s)?;
    wasi::filesystem::preopens::add_to_linker::<_, HasFs>(l, |s| s)?;
    Ok(())
}

struct HasFs;
impl HasData for HasFs {
    type Data<'a> = &'a mut HostState;
}

fn unsupported<T>() -> Result<std::result::Result<T, ErrorCode>> {
    Ok(Err(ErrorCode::Unsupported))
}

impl wasi::filesystem::preopens::Host for HostState {
    fn get_directories(&mut self) -> Result<Vec<(Resource<Descriptor>, String)>> {
        Ok(Vec::new())
    }
}

impl wasi::filesystem::types::Host for HostState {
    fn filesystem_error_code(
        &mut self,
        _err: Resource<wasmtime::Error>,
    ) -> Result<Option<ErrorCode>> {
        Ok(None)
    }
}

impl wasi::filesystem::types::HostDescriptor for HostState {
    fn read_via_stream(
        &mut self,
        _fd: Resource<Descriptor>,
        _offset: Filesize,
    ) -> Result<std::result::Result<Resource<DynInputStream>, ErrorCode>> {
        unsupported()
    }
    fn write_via_stream(
        &mut self,
        _fd: Resource<Descriptor>,
        _offset: Filesize,
    ) -> Result<std::result::Result<Resource<DynOutputStream>, ErrorCode>> {
        unsupported()
    }
    fn append_via_stream(
        &mut self,
        _fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<Resource<DynOutputStream>, ErrorCode>> {
        unsupported()
    }
    fn advise(
        &mut self,
        _fd: Resource<Descriptor>,
        _offset: Filesize,
        _len: Filesize,
        _advice: Advice,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn sync_data(
        &mut self,
        _fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn get_flags(
        &mut self,
        _fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<DescriptorFlags, ErrorCode>> {
        unsupported()
    }
    fn get_type(
        &mut self,
        _fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<DescriptorType, ErrorCode>> {
        unsupported()
    }
    fn set_size(
        &mut self,
        _fd: Resource<Descriptor>,
        _size: Filesize,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        unsupported()
    }
    fn set_times(
        &mut self,
        _fd: Resource<Descriptor>,
        _atim: NewTimestamp,
        _mtim: NewTimestamp,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn read(
        &mut self,
        _fd: Resource<Descriptor>,
        _len: Filesize,
        _offset: Filesize,
    ) -> Result<std::result::Result<(Vec<u8>, bool), ErrorCode>> {
        unsupported()
    }
    fn write(
        &mut self,
        _fd: Resource<Descriptor>,
        _buf: Vec<u8>,
        _offset: Filesize,
    ) -> Result<std::result::Result<Filesize, ErrorCode>> {
        unsupported()
    }
    fn read_directory(
        &mut self,
        _fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<Resource<DirEntryStream>, ErrorCode>> {
        unsupported()
    }
    fn sync(&mut self, _fd: Resource<Descriptor>) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn create_directory_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        unsupported()
    }
    fn stat(
        &mut self,
        _fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<DescriptorStat, ErrorCode>> {
        unsupported()
    }
    fn stat_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        _path: String,
    ) -> Result<std::result::Result<DescriptorStat, ErrorCode>> {
        unsupported()
    }
    fn set_times_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        _path: String,
        _atim: NewTimestamp,
        _mtim: NewTimestamp,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn link_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _old_path_flags: PathFlags,
        _old_path: String,
        _new_descriptor: Resource<Descriptor>,
        _new_path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        unsupported()
    }
    fn open_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        _path: String,
        _oflags: OpenFlags,
        _flags: DescriptorFlags,
    ) -> Result<std::result::Result<Resource<Descriptor>, ErrorCode>> {
        unsupported()
    }
    fn readlink_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path: String,
    ) -> Result<std::result::Result<String, ErrorCode>> {
        unsupported()
    }
    fn remove_directory_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        unsupported()
    }
    fn rename_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _old_path: String,
        _new_fd: Resource<Descriptor>,
        _new_path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        unsupported()
    }
    fn symlink_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _src_path: String,
        _dest_path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        unsupported()
    }
    fn unlink_file_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        unsupported()
    }
    fn is_same_object(
        &mut self,
        _fd: Resource<Descriptor>,
        _other: Resource<Descriptor>,
    ) -> Result<bool> {
        Ok(false)
    }
    fn metadata_hash(
        &mut self,
        _fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<MetadataHashValue, ErrorCode>> {
        unsupported()
    }
    fn metadata_hash_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        _path: String,
    ) -> Result<std::result::Result<MetadataHashValue, ErrorCode>> {
        unsupported()
    }
    fn drop(&mut self, fd: Resource<Descriptor>) -> Result<()> {
        self.table().delete(fd)?;
        Ok(())
    }
}

impl wasi::filesystem::types::HostDirectoryEntryStream for HostState {
    fn read_directory_entry(
        &mut self,
        _stream: Resource<DirEntryStream>,
    ) -> Result<std::result::Result<Option<DirectoryEntry>, ErrorCode>> {
        unsupported()
    }
    fn drop(&mut self, stream: Resource<DirEntryStream>) -> Result<()> {
        self.table().delete(stream)?;
        Ok(())
    }
}
