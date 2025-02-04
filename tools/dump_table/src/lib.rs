pub use samply_symbols::debugid;
use samply_symbols::debugid::DebugId;
use samply_symbols::{
    self, CandidatePathInfo, CompactSymbolTable, Error, FileAndPathHelper, FileAndPathHelperResult,
    FileLocation, LibraryInfo, MultiArchDisambiguator, OptionallySendFuture, SymbolManager,
};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;

#[cfg(feature = "chunked_caching")]
use samply_symbols::{FileByteSource, FileContents};

pub async fn get_table_for_binary(
    binary_path: &Path,
    debug_id: Option<DebugId>,
) -> Result<CompactSymbolTable, Error> {
    let helper = Helper {
        symbol_directory: binary_path.parent().unwrap().to_path_buf(),
    };
    let symbol_manager = SymbolManager::with_helper(&helper);
    let binary = symbol_manager
        .load_binary_at_path(binary_path, debug_id.map(MultiArchDisambiguator::DebugId))
        .await?;
    let info = binary.library_info();
    drop(binary);
    let symbol_map = symbol_manager.load_symbol_map(&info).await?;
    Ok(CompactSymbolTable::from_symbol_map(&symbol_map))
}

pub async fn get_table_for_debug_name_and_id(
    debug_name: &str,
    debug_id: Option<DebugId>,
    symbol_directory: PathBuf,
) -> Result<CompactSymbolTable, Error> {
    let helper = Helper { symbol_directory };
    let symbol_manager = SymbolManager::with_helper(&helper);
    let info = LibraryInfo {
        debug_name: Some(debug_name.to_string()),
        debug_id,
        ..Default::default()
    };
    let symbol_map = symbol_manager.load_symbol_map(&info).await?;
    Ok(CompactSymbolTable::from_symbol_map(&symbol_map))
}

pub fn dump_table(w: &mut impl Write, table: CompactSymbolTable, full: bool) -> anyhow::Result<()> {
    let mut w = BufWriter::new(w);
    writeln!(w, "Found {} symbols.", table.addr.len())?;
    for (i, address) in table.addr.iter().enumerate() {
        if i >= 15 && !full {
            writeln!(
                w,
                "and {} more symbols. Pass --full to print the full list.",
                table.addr.len() - i
            )?;
            break;
        }

        let start_pos = table.index[i];
        let end_pos = table.index[i + 1];
        let symbol_bytes = &table.buffer[start_pos as usize..end_pos as usize];
        let symbol_string = std::str::from_utf8(symbol_bytes)?;
        writeln!(w, "{:x} {}", address, symbol_string)?;
    }
    Ok(())
}

struct Helper {
    symbol_directory: PathBuf,
}

#[cfg(feature = "chunked_caching")]
struct MmapFileContents(memmap2::Mmap);

#[cfg(feature = "chunked_caching")]
impl FileByteSource for MmapFileContents {
    fn read_bytes_into(
        &self,
        buffer: &mut Vec<u8>,
        offset: u64,
        size: usize,
    ) -> FileAndPathHelperResult<()> {
        self.0.read_bytes_into(buffer, offset, size)
    }
}

#[cfg(feature = "chunked_caching")]
type FileContentsType = samply_symbols::FileContentsWithChunkedCaching<MmapFileContents>;

#[cfg(feature = "chunked_caching")]
fn mmap_to_file_contents(mmap: memmap2::Mmap) -> FileContentsType {
    samply_symbols::FileContentsWithChunkedCaching::new(mmap.len() as u64, MmapFileContents(mmap))
}

#[cfg(not(feature = "chunked_caching"))]
type FileContentsType = memmap2::Mmap;

#[cfg(not(feature = "chunked_caching"))]
fn mmap_to_file_contents(m: memmap2::Mmap) -> FileContentsType {
    m
}

impl<'h> FileAndPathHelper<'h> for Helper {
    type F = FileContentsType;
    type OpenFileFuture =
        Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Self::F>> + 'h>>;

    fn get_candidate_paths_for_debug_file(
        &self,
        library_info: &LibraryInfo,
    ) -> FileAndPathHelperResult<Vec<CandidatePathInfo>> {
        let debug_name = match library_info.debug_name.as_deref() {
            Some(debug_name) => debug_name,
            None => return Ok(Vec::new()),
        };

        let mut paths = vec![];

        // Also consider .so.dbg files in the symbol directory.
        if debug_name.ends_with(".so") {
            let debug_debug_name = format!("{}.dbg", debug_name);
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                self.symbol_directory.join(debug_debug_name),
            )));
        }

        // And dSYM packages.
        if !debug_name.ends_with(".pdb") {
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                self.symbol_directory
                    .join(format!("{}.dSYM", debug_name))
                    .join("Contents")
                    .join("Resources")
                    .join("DWARF")
                    .join(debug_name),
            )));
        }

        // Finally, the file itself.
        paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
            self.symbol_directory.join(debug_name),
        )));

        // For macOS system libraries, also consult the dyld shared cache.
        if self.symbol_directory.starts_with("/usr/")
            || self.symbol_directory.starts_with("/System/")
        {
            if let Some(dylib_path) = self.symbol_directory.join(debug_name).to_str() {
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_arm64e")
                        .to_path_buf(),
                    dylib_path: dylib_path.to_string(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64h")
                        .to_path_buf(),
                    dylib_path: dylib_path.to_string(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64")
                        .to_path_buf(),
                    dylib_path: dylib_path.to_string(),
                });
            }
        }

        Ok(paths)
    }

    fn get_candidate_paths_for_binary(
        &self,
        library_info: &LibraryInfo,
    ) -> FileAndPathHelperResult<Vec<CandidatePathInfo>> {
        let name = match library_info.name.as_deref() {
            Some(name) => name,
            None => return Ok(Vec::new()),
        };

        let mut paths = vec![];

        // Start with the file itself.
        paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
            self.symbol_directory.join(name),
        )));

        // For macOS system libraries, also consult the dyld shared cache.
        if self.symbol_directory.starts_with("/usr/")
            || self.symbol_directory.starts_with("/System/")
        {
            if let Some(dylib_path) = self.symbol_directory.join(name).to_str() {
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_arm64e")
                        .to_path_buf(),
                    dylib_path: dylib_path.to_string(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64h")
                        .to_path_buf(),
                    dylib_path: dylib_path.to_string(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64")
                        .to_path_buf(),
                    dylib_path: dylib_path.to_string(),
                });
            }
        }

        Ok(paths)
    }

    fn open_file(
        &'h self,
        location: &FileLocation,
    ) -> Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Self::F>> + 'h>> {
        async fn open_file_impl(path: PathBuf) -> FileAndPathHelperResult<FileContentsType> {
            eprintln!("Opening file {:?}", &path);
            let file = File::open(&path)?;
            let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
            Ok(mmap_to_file_contents(mmap))
        }

        let path = match location {
            FileLocation::Path(path) => path.clone(),
            FileLocation::Custom(_) => panic!("Unexpected FileLocation::Custom"),
        };
        Box::pin(open_file_impl(path))
    }

    fn get_dyld_shared_cache_paths(
        &self,
        _arch: Option<&str>,
    ) -> FileAndPathHelperResult<Vec<PathBuf>> {
        Ok(vec![])
    }
}
