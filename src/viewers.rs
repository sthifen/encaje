//! Quién usa el monitor virtual. Hay un solo monitor y un solo vídeo: con dos
//! tablets a la vez se pelearían por la resolución y la lenta frenaría a la
//! rápida, porque el bitrate es el del visor más lento. Así que tiene un solo
//! dueño, y cambiar de dueño es una decisión de quien usa las tablets:
//!
//! * Elegir a mano un PC en la app (`pick`) siempre se queda con el monitor.
//! * Conectar solo (`auto`) no le quita el monitor a nadie, salvo el cable a
//!   un WiFi que también entró solo: el USB es más rápido y enchufarlo es
//!   querer usarlo. Lo elegido a mano no lo desplaza ningún automatismo.
//!
//! Los visores sin identificar (el navegador, una app antigua, las pruebas de
//! carga) no compiten: ven el vídeo y sólo mandan si no hay dueño.

use std::sync::atomic::{AtomicU64, Ordering};

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
fn takes_over(owner: &Owner, client: &str, link: Link, picked: bool) -> bool {
    owner.client == client || picked || (link == Link::Usb && owner.link == Link::Wifi && !owner.picked)
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
    pub fn claim(&self, client: &str, name: &str, link: Link, picked: bool) -> Result<u64, String> {
        let session = self.sessions.fetch_add(1, Ordering::Relaxed) + 1;
        let mut result = Ok(session);
        self.owner.send_if_modified(|owner| match owner {
            Some(o) if !takes_over(o, client, link, picked) => {
                result = Err(o.name.clone());
                false
            }
            _ => {
                let picked = picked || owner.as_ref().is_some_and(|o| o.client == client && o.picked);
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

    /// Si ese visor puede mandar (`None`: sin identificar).
    pub fn may_control(&self, client: Option<&str>) -> bool {
        self.owner.borrow().as_ref().is_none_or(|o| client == Some(o.client.as_str()))
    }

    pub fn watch(&self) -> watch::Receiver<Option<Owner>> {
        self.owner.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_viewers_wait_their_turn() {
        let v = Viewers::new();
        assert!(v.claim("a", "A", Link::Wifi, false).is_ok());
        assert_eq!(v.claim("b", "B", Link::Wifi, false), Err("A".into()));
    }

    #[test]
    fn plugging_in_usb_beats_automatic_wifi() {
        let v = Viewers::new();
        v.claim("slow", "Vieja", Link::Wifi, false).unwrap();
        assert!(v.claim("fast", "Principal", Link::Usb, false).is_ok());
        assert_eq!(v.claim("slow", "Vieja", Link::Wifi, false), Err("Principal".into()));
    }

    #[test]
    fn a_manual_choice_is_not_overridden_automatically() {
        let v = Viewers::new();
        v.claim("slow", "Vieja", Link::Wifi, true).unwrap();
        assert_eq!(v.claim("fast", "Principal", Link::Usb, false), Err("Vieja".into()));
        assert!(v.claim("fast", "Principal", Link::Usb, true).is_ok());
    }

    #[test]
    fn reconnecting_keeps_the_manual_choice() {
        let v = Viewers::new();
        v.claim("a", "A", Link::Wifi, true).unwrap();
        v.claim("a", "A", Link::Wifi, false).unwrap();
        assert_eq!(v.claim("b", "B", Link::Usb, false), Err("A".into()));
    }

    #[test]
    fn an_old_connection_closing_does_not_free_the_new_one() {
        let v = Viewers::new();
        let old = v.claim("a", "A", Link::Wifi, false).unwrap();
        let new = v.claim("a", "A", Link::Wifi, false).unwrap();
        v.release(old);
        assert!(!v.may_control(None));
        v.release(new);
        assert!(v.may_control(None));
    }

    #[test]
    fn only_the_owner_controls_once_there_is_one() {
        let v = Viewers::new();
        assert!(v.may_control(None));
        v.claim("a", "A", Link::Usb, false).unwrap();
        assert!(v.may_control(Some("a")));
        assert!(!v.may_control(Some("b")));
        assert!(!v.may_control(None));
    }
}
