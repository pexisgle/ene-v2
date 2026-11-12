//! Idempotent acceptance receipts for chat and work delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptKind {
    Chat,
}

impl ReceiptKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptStatus {
    Pending,
    Accepted,
    NotDelivered,
}

impl ReceiptStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::NotDelivered => "not_delivered",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "accepted" => Some(Self::Accepted),
            "not_delivered" => Some(Self::NotDelivered),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Receipt {
    pub(crate) hash: String,
    pub(crate) status: ReceiptStatus,
    pub(crate) request_id: String,
    pub(crate) updated_at: String,
}
