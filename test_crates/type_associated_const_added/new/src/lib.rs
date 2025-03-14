#![no_std]

pub struct PublicStruct;

impl PublicStruct {
    // This is a new public associated constant, so it should trigger this lint.
    pub const NewConst: i32 = 0;
}
