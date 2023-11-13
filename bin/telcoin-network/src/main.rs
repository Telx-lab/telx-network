// We use jemalloc for performance reasons
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn main() {
    if let Err(err) = telcoin_network::cli::run() {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
