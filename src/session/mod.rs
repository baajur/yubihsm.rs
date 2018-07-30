use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

#[macro_use]
mod error;

pub use self::error::{SessionError, SessionErrorKind};
use auth_key::AuthKey;
use commands::{close_session::CloseSessionCommand, create_session::create_session, Command};
use connector::{Connector, HttpConfig, HttpConnector, Status as ConnectorStatus};
use object::ObjectId;
use securechannel::SessionId;
use securechannel::{Challenge, Channel, CommandMessage, ResponseCode, ResponseMessage};
use serializers::deserialize;

/// Sessions with the YubiHSM2 are stateful and expire after 30 seconds. See:
/// <https://developers.yubico.com/YubiHSM2/Concepts/Session.html>
pub const SESSION_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout fuzz factor: to avoid races/skew with the YubiHSM2's clock,
/// we consider sessions to be timed out slightly earlier than the actual
/// timeout. This should (hopefully) ensure we always time out first.
const TIMEOUT_SKEW_INTERVAL: Duration = Duration::from_secs(1);

/// Status message returned from healthy connectors
const CONNECTOR_STATUS_OK: &str = "OK";

/// Write consistent `debug!(...) lines for sessions
macro_rules! session_debug {
    ($session:expr, $msg:expr) => {
        debug!("yubihsm: session={} {}", $session.id.to_u8(), $msg);
    };
    ($session:expr, $fmt:expr, $($arg:tt)+) => {
        debug!(concat!("yubihsm: session={} ", $fmt), $session.id.to_u8(), $($arg)+);
    };
}

/// Encrypted session with the `YubiHSM2`.
/// A session is needed to perform any commands.
///
/// Sessions are eneric over `Connector` types in case a different one needs to
/// be swapped in, which is primarily useful for substituting the `MockHSM`.
///
/// Sessions are automatically closed on `Drop`, releasing `YubiHSM2` session
/// resources and wiping the ephemeral keys used to encrypt the session.
pub struct Session<C = HttpConnector>
where
    C: Connector,
{
    /// ID of this session
    id: SessionId,

    /// Encrypted channel to the HSM
    channel: Channel,

    /// Connector to send messages through
    connector: C,

    /// Instant when the last command with the YubiHSM2 was sent. Used for
    /// tracking session inactivity timeouts
    last_command_timestamp: Instant,

    /// Optional cached `AuthKey` for reconnecting lost sessions
    // TODO: session reconnect support
    #[allow(dead_code)]
    auth_key: Option<AuthKey>,
}

// Special casing these for HttpConnector is a bit of a hack in that default
// generics and static methods do not play well together, e.g.
//
// error[E0283]: type annotations required: cannot resolve `yubihsm::Connector`
//
// So we special case these for HttpConnector to make the API more ergonomic
impl Session<HttpConnector> {
    /// Open a new session to the HSM, authenticating with the given `AuthKey`
    pub fn create(
        connector_config: HttpConfig,
        auth_key_id: ObjectId,
        auth_key: AuthKey,
        reconnect: bool,
    ) -> Result<Self, SessionError> {
        let connector_info = connector_config.to_string();
        let connector = HttpConnector::open(connector_config)?;
        let status = connector.status()?;

        if status.message != CONNECTOR_STATUS_OK {
            session_fail!(
                CreateFailed,
                "bad status response from {}: {}",
                connector_info,
                status.message
            );
        }

        Self::new(connector, auth_key_id, auth_key, reconnect)
    }

    /// Open a new session to the HSM, authenticating with a given password.
    /// Uses the same password-based key derivation method as yubihsm-shell
    /// (PBKDF2 + static salt), which is not particularly strong, so use
    /// of a long, random password is recommended.
    #[cfg(feature = "passwords")]
    pub fn create_from_password(
        connector_config: HttpConfig,
        auth_key_id: ObjectId,
        password: &[u8],
        reconnect: bool,
    ) -> Result<Self, SessionError> {
        Self::create(
            connector_config,
            auth_key_id,
            AuthKey::derive_from_password(password),
            reconnect,
        )
    }
}

impl<C: Connector> Session<C> {
    /// Create a new encrypted session using the given connector, YubiHSM2 auth key ID, and
    /// authentication key
    pub fn new(
        connector: C,
        auth_key_id: ObjectId,
        auth_key: AuthKey,
        reconnect: bool,
    ) -> Result<Self, SessionError> {
        debug!("yubihsm: creating new session");

        let host_challenge = Challenge::random();

        let (session_id, session_response) =
            create_session(&connector, auth_key_id, host_challenge)?;

        let channel = Channel::new(
            session_id,
            &auth_key,
            host_challenge,
            session_response.card_challenge,
        );

        if channel
            .card_cryptogram()
            .ct_eq(&session_response.card_cryptogram)
            .unwrap_u8() != 1
        {
            session_fail!(AuthFailed, "card cryptogram mismatch!");
        }

        let mut session = Self {
            id: session_id,
            channel,
            connector,
            last_command_timestamp: Instant::now(),
            auth_key: if reconnect { Some(auth_key) } else { None },
        };

        session_debug!(
            session,
            "authenticating session with key ID: {}",
            auth_key_id
        );

        session.authenticate()?;

        session_debug!(session, "session authenticated successfully");

        Ok(session)
    }

    /// Get the current session ID
    #[inline]
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Request current yubihsm-connector status
    pub fn connector_status(&mut self) -> Result<ConnectorStatus, SessionError> {
        self.connector.status().map_err(|e| e.into())
    }

    /// Authenticate the current session with the `YubiHSM2`
    fn authenticate(&mut self) -> Result<(), SessionError> {
        let command = self.channel.authenticate_session()?;
        let response = self.send_command(command)?;
        self.channel
            .finish_authenticate_session(&response)
            .map_err(|e| e.into())
    }

    /// Send a command message to the YubiHSM2 and parse the response
    fn send_command(&mut self, cmd: CommandMessage) -> Result<ResponseMessage, SessionError> {
        let time_since_last_command = Instant::now().duration_since(self.last_command_timestamp);
        // TODO: handle reconnecting when sessions are lost
        if time_since_last_command > (SESSION_INACTIVITY_TIMEOUT - TIMEOUT_SKEW_INTERVAL) {
            let msg = format!(
                "session timed out after {} seconds (max {})",
                time_since_last_command.as_secs(),
                SESSION_INACTIVITY_TIMEOUT.as_secs()
            );

            session_debug!(self, &msg);
            session_fail!(TimeoutError, msg);
        }

        let cmd_type = cmd.command_type;
        let uuid = cmd.uuid;

        session_debug!(self, "uuid={} command={:?}", &uuid, cmd_type);
        let response_bytes = self.connector.send_command(uuid, cmd.into())?;

        let response = ResponseMessage::parse(response_bytes)?;

        session_debug!(
            self,
            "uuid={} response={:?} length={}",
            &uuid,
            response.code,
            response.data.len()
        );

        self.last_command_timestamp = Instant::now();

        if response.is_err() {
            session_fail!(ResponseError, "HSM error: {:?}", response.code);
        }

        if response.command().unwrap() != cmd_type {
            session_fail!(
                ProtocolError,
                "command type mismatch: expected {:?}, got {:?}",
                cmd_type,
                response.command().unwrap()
            );
        }

        Ok(response)
    }

    /// Encrypt a command and send it to the card, then authenticate and
    /// decrypt the response
    pub(crate) fn send_encrypted_command<T: Command>(
        &mut self,
        command: T,
    ) -> Result<T::ResponseType, SessionError> {
        let encrypted_cmd = self.channel.encrypt_command(command.into())?;
        let uuid = encrypted_cmd.uuid;

        session_debug!(self, "uuid={} encrypted-cmd={:?}", uuid, T::COMMAND_TYPE);

        let encrypted_response = self.send_command(encrypted_cmd)?;
        let response = self.channel.decrypt_response(encrypted_response)?;

        session_debug!(
            self,
            "uuid={} decrypted-resp={:?} length={}",
            uuid,
            response.code,
            response.data.len()
        );

        if response.is_err() {
            // TODO: factor this into ResponseMessage or ResponseCode?
            let description = match response.code {
                ResponseCode::MemoryError => {
                    "general HSM error (e.g. bad command params, missing object)".to_owned()
                }
                other => format!("{:?}", other),
            };

            session_fail!(ResponseError, description);
        }

        if response.command().unwrap() != T::COMMAND_TYPE {
            session_fail!(
                ResponseError,
                "command type mismatch: expected {:?}, got {:?}",
                T::COMMAND_TYPE,
                response.command().unwrap()
            );
        }

        deserialize(response.data.as_ref()).map_err(|e| e.into())
    }
}

/// Close session automatically on drop
impl<C: Connector> Drop for Session<C> {
    /// Make a best effort to close the session
    ///
    /// NOTE: this runs the potential of panicking in a drop handler, which
    /// results in the following when it occurs (Aieee!):
    ///
    /// "thread panicked while panicking. aborting"
    ///
    /// Because of this, it's very important `send_encrypted_command` and
    /// everything it calls be panic-free.
    fn drop(&mut self) {
        session_debug!(self, "closing dropped session");

        // TODO: only attempt to do this if the connection state is healthy
        if let Err(e) = self.send_encrypted_command(CloseSessionCommand {}) {
            session_debug!(self, "error closing dropped session: {}", e);
        }
    }
}
