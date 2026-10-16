//! Pi and Oh My Pi (OMP) speak nearly the same RPC; the differences live here.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavor {
    Pi,
    Omp,
}

impl Flavor {
    /// Provider id used in the agent-level surface.
    pub fn provider_id(self) -> &'static str {
        match self {
            Flavor::Pi => "pi",
            Flavor::Omp => "omp",
        }
    }

    /// Human label used in permission names and diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Flavor::Pi => "Pi",
            Flavor::Omp => "OMP",
        }
    }

    pub fn diagnostic_name(self) -> &'static str {
        match self {
            Flavor::Pi => "Pi RPC",
            Flavor::Omp => "OMP RPC",
        }
    }

    /// RPC command that lists slash commands.
    pub fn commands_rpc_name(self) -> &'static str {
        match self {
            Flavor::Pi => "get_commands",
            Flavor::Omp => "get_available_commands",
        }
    }

    /// OMP announces itself with a `ready` frame and may negotiate protocol v2.
    pub fn waits_for_ready(self) -> bool {
        matches!(self, Flavor::Omp)
    }

    /// Binary to run when the caller did not pick one. Mirrors the TS env overrides.
    pub fn default_command(self) -> Vec<String> {
        let from_env = match self {
            Flavor::Pi => std::env::var("PI_COMMAND")
                .or_else(|_| std::env::var("PI_ACP_PI_COMMAND"))
                .ok(),
            Flavor::Omp => std::env::var("OMP_COMMAND").ok(),
        };
        let fallback = match self {
            Flavor::Pi => "pi",
            Flavor::Omp => "omp",
        };
        vec![from_env.unwrap_or_else(|| fallback.to_string())]
    }
}
