use data::subgraph::Link;
use failure;
use futures::prelude::*;
use ipfs_api;

/// Resolves links to subgraph manifests and resources referenced by them.
pub trait LinkResolver: Send + Sync + 'static {
    /// Fetches the link contents as bytes.
    fn cat(&self, link: &Link) -> Box<Future<Item = Vec<u8>, Error = failure::Error> + Send>;
}

impl LinkResolver for ipfs_api::IpfsClient {
    /// Currently supports only links of the form `/ipfs/ipfs_hash`
    fn cat(&self, link: &Link) -> Box<Future<Item = Vec<u8>, Error = failure::Error> + Send> {
        let link = &link.link;
        // Verify that the link is in the expected form `/ipfs/hash`.
        if !link.starts_with("/ipfs/") {
            return Box::new(Err(failure::err_msg(format!("Invalid link {}", link))).into_future());
        }

        // Discard the `/ipfs/` prefix to get the hash.
        let path = link.trim_left_matches("/ipfs/");

        Box::new(
            self.cat(path)
                .concat2()
                .map(|x| x.to_vec())
                .map_err(|e| failure::Error::from(e)),
        )
    }
}
