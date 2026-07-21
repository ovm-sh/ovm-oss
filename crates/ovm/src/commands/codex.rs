use crate::error::Result;
use crate::product::Product;

pub fn run(args: &[String]) -> Result<()> {
    super::launch::run(Product::Codex, args)
}
