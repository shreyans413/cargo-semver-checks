#![no_std]

#[non_exhaustive]
pub enum NonExhaustiveEnum {
    VariantA,

    #[non_exhaustive]
    VariantB,
}