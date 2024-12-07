use clap::{Args, Parser as _};
#[cfg(feature = "faucet")]
use tn_faucet::FaucetArgs;
use tn_node::launch_node;

// We use jemalloc for performance reasons
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// No Additional arguments
#[derive(Debug, Clone, Copy, Default, Args)]
#[non_exhaustive]
pub struct NoArgs;

fn main() {
    #[cfg(not(feature = "faucet"))]
    if let Err(err) = telcoin_network::cli::Cli::<NoArgs>::parse()
        .run(|builder, _, tn_datadir| async move { launch_node(builder, tn_datadir).await })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }

    #[cfg(feature = "faucet")]
    if let Err(err) = telcoin_network::cli::Cli::<FaucetArgs>::parse().run(
        |mut builder, faucet, tn_datadir| async move {
            builder.opt_faucet_args = Some(faucet);
            launch_node(builder, tn_datadir).await
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
