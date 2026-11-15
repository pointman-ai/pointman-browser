//! agent-browser (github.com/vercel-labs/agent-browser) on Pointman's machines: `install` puts its pinned release
//! in the extension's state folder, and `cli` runs one call and reads its answer. `login` reads a test login, `vars`
//! puts its values into a run's args, and `signin` is what the sign-in pane decides without a browser.

pub mod cli;
pub mod install;
pub mod login;
pub mod signin;
pub mod test;
pub mod vars;
