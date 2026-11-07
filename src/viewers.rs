//! Quién usa el monitor virtual. Hay un solo monitor y un solo vídeo: con dos
//! tablets a la vez se pelearían por la resolución y la lenta frenaría a la
//! rápida, porque el bitrate es el del visor más lento. Así que tiene un solo
//! dueño, y cambiar de dueño es una decisión de quien usa las tablets:
//!
//! * Elegir a mano un PC en la app (`ask`) lo usa si está libre; si no, la app
//!   pregunta, y sólo responder «usar en esta tablet» (`pick`) lo quita.
//! * Conectar solo (`auto`) no le quita el monitor a nadie, salvo el cable a
//!   un WiFi que también entró solo: el USB es más rápido y enchufarlo es
//!   querer usarlo. Lo elegido a mano no lo desplaza ningún automatismo.
//!
//! Los visores sin identificar (el navegador, una app antigua, las pruebas de
//! carga) no compiten: ven el vídeo y sólo mandan si no hay dueño.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Usb,
    Wifi,
}

impl Link {
    fn label(self) -> &'static str {
        match self {
            Link::Usb => "USB",
            Link::Wifi => "WiFi",
        }
    }
}

/// Cómo pide el monitor un visor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// Conectó solo.
    Auto,
    /// La persona eligió este PC: cuenta como elección, pero no quita a nadie.
    Ask,
    /// La persona pidió quedárselo aunque lo use otro.
    Pick,
}

impl Claim {
    pub fn parse(mode: Option<&str>) -> Self {
        match mode {
            Some("pick") => Claim::Pick,
            Some("ask") => Claim::Ask,
            _ => Claim::Auto,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub client: String,
    pub name: String,
    pub link: Link,
    /// Se eligió a mano.
    pub picked: bool,
    /// Esta conexión: al cerrarse una vieja no debe soltar la nueva.
    pub session: u64,
}

impl Owner {
    /// Cómo se muestra en la ventana del PC.
    pub fn label(&self) -> String {
        format!("{} por {}", self.name, self.link.label())
    }
}

/// Si `new` se queda con el monitor de `owner`.
fn takes_over(owner: &Owner, client: &str, link: Link, claim: Claim) -> bool {
    owner.client == client
        || claim == Claim::Pick
        || (claim == Claim::Auto && link == Link::Usb && owner.link == Link::Wifi && !owner.picked)
}

pub struct Viewers {
    owner: watch::Sender<Option<Owner>>,
    sessions: AtomicU64,
}

impl Viewers {
    pub fn new() -> Self {
        Self { owner: watch::Sender::new(None), sessions: AtomicU64::new(0) }
    }

    /// Pide el monitor. Devuelve la sesión, o el nombre de quien lo usa.
    pub fn claim(&self, client: &str, name: &str, link: Link, claim: Claim) -> Result<u64, String> {
        let session = self.sessions.fetch_add(1, Ordering::Relaxed) + 1;
        let mut result = Ok(session);
        self.owner.send_if_modified(|owner| match owner {
            Some(o) if !takes_over(o, client, link, claim) => {
                result = Err(o.name.clone());
                false
            }
            _ => {
                let picked = claim != Claim::Auto || owner.as_ref().is_some_and(|o| o.client == client && o.picked);
                *owner = Some(Owner { client: client.into(), name: name.into(), link, picked, session });
                true
            }
        });
        result
    }

    /// Suelta el monitor si esta sesión sigue siendo la dueña.
    pub fn release(&self, session: u64) {
        self.owner.send_if_modified(|owner| {
            let mine = owner.as_ref().is_some_and(|o| o.session == session);
            if mine {
                *owner = None;
            }
            mine
        });
    }

    /// Nombre de quien usa el monitor ahora.
    pub fn owner_name(&self) -> Option<String> {
        self.owner.borrow().as_ref().map(|o| o.name.clone())
    }

    /// Si ese visor puede mandar (`None`: sin identificar).
    pub fn may_control(&self, client: Option<&str>) -> bool {
        self.owner.borrow().as_ref().is_none_or(|o| client == Some(o.client.as_str()))
    }

    pub fn watch(&self) -> watch::Receiver<Option<Owner>> {
        self.owner.subscribe()
    }

    /// Termina cuando lleva `grace` sin que nadie use el monitor. El plazo
    /// deja reconectar a una tablet que pierde la WiFi un momento.
    pub async fn wait_idle(&self, grace: Duration) {
        let mut owner = self.watch();
        loop {
            if owner.wait_for(Option::is_none).await.is_err() {
                return;
            }
            if tokio::time::timeout(grace, owner.wait_for(Option::is_some)).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_viewers_wait_their_turn() {
        let v = Viewers::new();
        assert!(v.claim("a", "A", Link::Wifi, Claim::Auto).is_ok());
        assert_eq!(v.claim("b", "B", Link::Wifi, Claim::Auto), Err("A".into()));
    }

    #[test]
    fn plugging_in_usb_beats_automatic_wifi() {
        let v = Viewers::new();
        v.claim("slow", "Vieja", Link::Wifi, Claim::Auto).unwrap();
        assert!(v.claim("fast", "Principal", Link::Usb, Claim::Auto).is_ok());
        assert_eq!(v.claim("slow", "Vieja", Link::Wifi, Claim::Auto), Err("Principal".into()));
    }

    #[test]
    fn a_manual_choice_is_not_overridden_automatically() {
        let v = Viewers::new();
        v.claim("slow", "Vieja", Link::Wifi, Claim::Pick).unwrap();
        assert_eq!(v.claim("fast", "Principal", Link::Usb, Claim::Auto), Err("Vieja".into()));
        assert!(v.claim("fast", "Principal", Link::Usb, Claim::Pick).is_ok());
    }

    #[test]
    fn asking_uses_a_free_monitor_but_never_takes_one() {
        let v = Viewers::new();
        v.claim("a", "A", Link::Wifi, Claim::Auto).unwrap();
        assert_eq!(v.claim("b", "B", Link::Usb, Claim::Ask), Err("A".into()));
        assert!(v.claim("b", "B", Link::Usb, Claim::Pick).is_ok());

        let v = Viewers::new();
        v.claim("a", "A", Link::Usb, Claim::Ask).unwrap();
        // Cuenta como elegido a mano: el cable automático no lo desplaza.
        assert_eq!(v.claim("b", "B", Link::Usb, Claim::Auto), Err("A".into()));
    }

    #[test]
    fn reconnecting_keeps_the_manual_choice() {
        let v = Viewers::new();
        v.claim("a", "A", Link::Wifi, Claim::Pick).unwrap();
        v.claim("a", "A", Link::Wifi, Claim::Auto).unwrap();
        assert_eq!(v.claim("b", "B", Link::Usb, Claim::Auto), Err("A".into()));
    }

    #[test]
    fn an_old_connection_closing_does_not_free_the_new_one() {
        let v = Viewers::new();
        let old = v.claim("a", "A", Link::Wifi, Claim::Auto).unwrap();
        let new = v.claim("a", "A", Link::Wifi, Claim::Auto).unwrap();
        v.release(old);
        assert!(!v.may_control(None));
        v.release(new);
        assert!(v.may_control(None));
    }

    #[test]
    fn only_the_owner_controls_once_there_is_one() {
        let v = Viewers::new();
        assert!(v.may_control(None));
        v.claim("a", "A", Link::Usb, Claim::Auto).unwrap();
        assert!(v.may_control(Some("a")));
        assert!(!v.may_control(Some("b")));
        assert!(!v.may_control(None));
    }
}
