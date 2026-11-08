#[derive(Default)]
struct PendingStreams {
    entries: HashMap<(String, Option<String>), PendingStream>,
    // A chat's admission generations live for the gateway lifetime, including
    // periods with no SSE receiver: already enqueued messages still carry them.
    admissions: HashMap<String, Arc<ApiAdmission>>,
}

struct PendingStream {
    context: TurnContext,
    threads: std::collections::HashSet<String>,
    sender: SseSender,
    owners: HashMap<String, SessionKey>,
}

impl PendingStream {
    fn context_for(&self, ctx: &TurnContext) -> TurnContext {
        TurnContext::new(
            ctx.session_id.clone(),
            self.context.topic.clone(),
            ctx.thread_id.clone(),
        )
    }
}

impl PendingStreams {
    fn key(ctx: &TurnContext) -> (String, Option<String>) {
        (ctx.session_id.clone(), ctx.topic.clone())
    }

    fn get(&self, ctx: &TurnContext) -> Option<&PendingStream> {
        self.entries.values().find(|stream| {
            stream.context.session_id == ctx.session_id
                && stream.threads.contains(ctx.thread_id_str())
        })
    }

    fn insert(&mut self, ctx: TurnContext, sender: SseSender) {
        let stream = self
            .entries
            .entry(Self::key(&ctx))
            .or_insert_with(|| PendingStream {
                context: ctx.clone(),
                threads: Default::default(),
                sender,
                owners: HashMap::new(),
            });
        stream.threads.insert(ctx.thread_id_str().to_owned());
    }

    fn remove(&mut self, ctx: &TurnContext) {
        if let Some(stream) = self.get(ctx) {
            let key = Self::key(&stream.context);
            self.entries.remove(&key);
        }
    }

    fn has_chat(&self, chat: &str) -> bool {
        self.entries.keys().any(|(id, _)| id == chat)
    }

    fn active(&self, chat: &str, topic: Option<&str>) -> bool {
        self.entries.keys().any(|(id, scope)| {
            id == chat && topic.is_none_or(|topic| scope.as_deref() == Some(topic))
        })
    }

    fn admission(&mut self, chat: &str) -> Arc<ApiAdmission> {
        self.admissions.entry(chat.to_owned()).or_default().clone()
    }

    fn session_keys(&self, profile_id: Option<&str>) -> Vec<SessionKey> {
        self.entries
            .values()
            .flat_map(|entry| {
                entry
                    .threads
                    .iter()
                    .map(|thread| entry.owner(profile_id, thread))
            })
            .collect()
    }

    fn remove_thread(&mut self, chat: &str, thread: &str) {
        self.entries.retain(|_, entry| {
            if entry.context.session_id == chat {
                entry.threads.remove(thread);
                entry.owners.remove(thread);
            }
            !entry.threads.is_empty()
        });
    }

    fn remove_sessions(&mut self, profile_id: Option<&str>, deleted: &[SessionKey]) {
        self.entries.retain(|_, entry| {
            let removed: Vec<_> = entry
                .threads
                .iter()
                .filter(|thread| {
                    let owner = entry.owner(profile_id, thread);
                    deleted
                        .iter()
                        .any(|key| session_delete_matches(key, &owner))
                })
                .cloned()
                .collect();
            for thread in removed {
                entry.threads.remove(&thread);
                entry.owners.remove(&thread);
            }
            !entry.threads.is_empty()
        });
    }
}

fn session_delete_matches(deleted: &SessionKey, owner: &SessionKey) -> bool {
    deleted.base_key() == owner.base_key()
        && (deleted.topic().is_none() || deleted.topic() == owner.topic())
}

impl PendingStream {
    fn owner(&self, profile_id: Option<&str>, thread: &str) -> SessionKey {
        self.owners.get(thread).cloned().unwrap_or_else(|| {
            current_profile_api_session_key_with_topic(
                profile_id,
                &self.context.session_id,
                self.context.topic.as_deref(),
            )
        })
    }
}

#[derive(Default)]
struct ApiAdmission {
    deletion: Mutex<()>,
    state: Arc<tokio::sync::RwLock<ApiAdmissionState>>,
}

#[derive(Default)]
struct ApiAdmissionState {
    generation: u64,
    deleting: Vec<SessionKey>,
    deleted: HashMap<SessionKey, u64>,
}

impl ApiAdmissionState {
    fn deleted_since(&self, generation: u64, owner: &SessionKey) -> bool {
        self.deleted
            .iter()
            .any(|(key, deleted)| *deleted > generation && session_delete_matches(key, owner))
    }

    fn record_deletion(&mut self, requested: &[SessionKey]) {
        self.generation += 1;
        for key in requested {
            self.deleted.insert(key.clone(), self.generation);
        }
    }
}
