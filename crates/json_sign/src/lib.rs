mod preloader;

use json_ld::RemoteDocument;
use json_syntax::{Parse, Value};
use locspan::Location;
use once_cell::sync::Lazy;
use preloader::PreLoaderFactory;
use rsa::RsaPrivateKey;
use rsa_signature_2017::json_ld::JsonLdOptions;
pub use rsa_signature_2017::Signature as RsaSignature;
use sophia_api::dataset::CollectibleDataset;
use sophia_inmem::dataset::LightDataset;
use sophia_iri::Iri;
use sophia_jsonld::JsonLdParser;
use std::sync::Arc;

static ACTIVITYSTREAMS: Lazy<JsonLdParser<PreLoaderFactory>> = Lazy::new(|| {
    JsonLdParser::new_with_options(
        JsonLdOptions::new().with_document_loader_factory(PreLoaderFactory),
    )
});

pub async fn get_sign<'a>(
    s: &str,
    key: &RsaPrivateKey,
    creator: &'a str,
) -> Option<RsaSignature<'a>> {
    let creator = Iri::new(creator).ok()?;
    let placeholder_iri = Iri::new_unchecked(Arc::from("urn:x-placeholder"));
    let json = Value::parse_str(s, |span| Location::new(placeholder_iri.clone(), span)).ok()?;
    let doc = RemoteDocument::new(None, None, json);
    let quads = ACTIVITYSTREAMS.parse_json(&doc).await;
    let mut sign_options = RsaSignature::options();
    let dataset = LightDataset::from_quad_source(quads).ok()?;
    sign_options
        .sign_rsa_signature_2017(&dataset, key, creator)
        .ok()
}
