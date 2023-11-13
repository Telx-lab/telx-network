use std::num::NonZeroUsize;
// Copyright (c) Telcoin, LLC
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use narwhal_storage::{CertificateStore, CertificateStoreCache, PayloadStore, PayloadToken};
use narwhal_typed_store::{
    reopen, rocks,
    rocks::{DBMap, MetricConf, ReadWriteOptions},
};
use narwhal_types::{
    test_utils::{
        temp_dir, CERTIFICATES_CF, CERTIFICATE_DIGEST_BY_ORIGIN_CF, CERTIFICATE_DIGEST_BY_ROUND_CF,
        PAYLOAD_CF,
    },
    AuthorityIdentifier, BatchDigest, Certificate, CertificateDigest, Round, WorkerId,
};

pub fn create_db_stores() -> (CertificateStore, PayloadStore) {
    // Create a new test store.
    let rocksdb = rocks::open_cf(
        temp_dir(),
        None,
        MetricConf::default(),
        &[
            CERTIFICATES_CF,
            CERTIFICATE_DIGEST_BY_ROUND_CF,
            CERTIFICATE_DIGEST_BY_ORIGIN_CF,
            PAYLOAD_CF,
        ],
    )
    .expect("Failed creating database");

    let (
        certificate_map,
        certificate_digest_by_round_map,
        certificate_digest_by_origin_map,
        payload_map,
    ) = reopen!(&rocksdb,
        CERTIFICATES_CF;<CertificateDigest, Certificate>,
        CERTIFICATE_DIGEST_BY_ROUND_CF;<(Round, AuthorityIdentifier), CertificateDigest>,
        CERTIFICATE_DIGEST_BY_ORIGIN_CF;<(AuthorityIdentifier, Round), CertificateDigest>,
        PAYLOAD_CF;<(BatchDigest, WorkerId), PayloadToken>);

    (
        CertificateStore::new(
            certificate_map,
            certificate_digest_by_round_map,
            certificate_digest_by_origin_map,
            CertificateStoreCache::new(NonZeroUsize::new(100).unwrap(), None),
        ),
        PayloadStore::new(payload_map),
    )
}
