"""Copy the released Oxigraph source and add experiment-only RocksDB switches."""
import os
from pathlib import Path
import shutil
import subprocess

root = Path(__file__).resolve().parents[2]
subprocess.run(["cargo", "info", "oxigraph@0.5.13"], check=True, stdout=subprocess.DEVNULL)
registry = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")) / "registry" / "src"
sources = list(registry.glob("*/oxigraph-0.5.13"))
if len(sources) != 1:
    raise RuntimeError(f"expected exactly one cached oxigraph-0.5.13 source, found {sources}")
target = root / "target" / "rocksdb-options" / "oxigraph"
if target.exists():
    raise RuntimeError(f"{target} already exists; remove it to prepare a fresh copy")
shutil.copytree(sources[0], target)
wrapper = target / "src" / "storage" / "rocksdb_wrapper.rs"
source = wrapper.read_text()
needle = "            rocksdb_options_set_compression(options, rocksdb_lz4_compression.try_into().unwrap());"
assert source.count(needle) == 1
addition = '''
            if std::env::var_os("EXPERIMENT_COMPRESS").is_some() {
                let levels = [rocksdb_lz4_compression as i32; 7];
                rocksdb_options_set_compression_per_level(options, levels.as_ptr(), levels.len());
            }
            if std::env::var_os("EXPERIMENT_SUBCOMPACTIONS").is_some() {
                rocksdb_options_set_max_subcompactions(options, 4);
            }
'''
wrapper.write_text(source.replace(needle, needle + addition))
