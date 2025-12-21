# vertex

It's really hard to stop cargo from rebuilding too much in CI, even with

  * Timestamp restoration with [timelord-cli](https://github.com/fasterthanlime/timelord)
  * Cache save/restore with [ctree](https://crates.io/crates/ctree) and [cargo-sweep](https://github.com/holmgr/cargo-sweep)
  * `-Z mtime-on-use` for trimming, `-Z checksum-freshness`, etc.
  
[buck2](https://buck2.build/) is great but a lot of effort.

Is there a middle ground? Let's find out.
