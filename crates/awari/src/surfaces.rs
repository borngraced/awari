//! Layer-shell roles. GPUI `WindowKind::LayerShell` uses these constants.

pub const LAUNCHER_NAMESPACE: &str = "awari:launcher";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceRole {
    Launcher,
}

impl SurfaceRole {
    pub fn namespace(self) -> &'static str {
        match self {
            Self::Launcher => LAUNCHER_NAMESPACE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_namespace() {
        assert_eq!(SurfaceRole::Launcher.namespace(), LAUNCHER_NAMESPACE);
    }
}
