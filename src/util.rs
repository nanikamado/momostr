use serde::Serialize;

#[derive(Serialize)]
pub struct Merge<T1, T2> {
    #[serde(flatten)]
    pub f1: T1,
    #[serde(flatten)]
    pub f2: T2,
}
