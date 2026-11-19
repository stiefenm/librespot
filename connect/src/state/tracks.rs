use crate::{
    core::{Error, SpotifyUri},
    protocol::player::ProvidedTrack,
    state::{
        ConnectState, SPOTIFY_MAX_NEXT_TRACKS_SIZE, SPOTIFY_MAX_PREV_TRACKS_SIZE, StateError,
        context::ContextType,
        metadata::Metadata,
        provider::{IsProvider, Provider},
    },
};
use protobuf::MessageField;
use rand::Rng;

// identifier used as part of the uid
pub const IDENTIFIER_DELIMITER: &str = "delimiter";

impl<'ct> ConnectState {
    fn new_delimiter(iteration: i64) -> ProvidedTrack {
        let mut delimiter = ProvidedTrack {
            uri: format!("spotify:{IDENTIFIER_DELIMITER}"),
            uid: format!("{IDENTIFIER_DELIMITER}{iteration}"),
            provider: Provider::Context.to_string(),
            ..Default::default()
        };
        delimiter.set_hidden(true);
        delimiter.set_iteration(iteration);

        delimiter
    }

    fn push_prev(&mut self, prev: ProvidedTrack) {
        let prev_tracks = self.prev_tracks_mut();
        // add prev track, while preserving a length of 10
        if prev_tracks.len() >= SPOTIFY_MAX_PREV_TRACKS_SIZE {
            // todo: O(n), but technically only maximal O(SPOTIFY_MAX_PREV_TRACKS_SIZE) aka O(10)
            let _ = prev_tracks.remove(0);
        }
        prev_tracks.push(prev)
    }

    fn get_next_track(&mut self) -> Option<ProvidedTrack> {
        if self.next_tracks().is_empty() {
            None
        } else {
            // todo: O(n), but technically only maximal O(SPOTIFY_MAX_NEXT_TRACKS_SIZE) aka O(80)
            Some(self.next_tracks_mut().remove(0))
        }
    }

    /// bottom => top, aka the last track of the list is the prev track
    fn prev_tracks_mut(&mut self) -> &mut Vec<ProvidedTrack> {
        &mut self.player_mut().prev_tracks
    }

    /// bottom => top, aka the last track of the list is the prev track
    pub(super) fn prev_tracks(&self) -> &Vec<ProvidedTrack> {
        &self.player().prev_tracks
    }

    /// top => bottom, aka the first track of the list is the next track
    fn next_tracks_mut(&mut self) -> &mut Vec<ProvidedTrack> {
        &mut self.player_mut().next_tracks
    }

    /// top => bottom, aka the first track of the list is the next track
    pub(super) fn next_tracks(&self) -> &Vec<ProvidedTrack> {
        &self.player().next_tracks
    }

    pub fn set_current_track_random(&mut self) -> Result<(), Error> {
        let max_tracks = self.get_context(self.active_context)?.tracks.len();
        let rng_track = rand::rng().random_range(0..max_tracks);
        self.set_current_track(rng_track)
    }

    pub fn set_current_track(&mut self, index: usize) -> Result<(), Error> {
        let context = self.get_context(self.active_context)?;

        let new_track = context
            .tracks
            .get(index)
            .ok_or(StateError::CanNotFindTrackInContext(
                Some(index),
                context.tracks.len(),
            ))?;

        debug!(
            "set track to: {} at {} of {} tracks",
            new_track.uri,
            index,
            context.tracks.len()
        );

        self.set_track(new_track.clone());

        self.update_current_index(|i| i.track = index as u32);

        Ok(())
    }

    /// Move to the next track
    ///
    /// Updates the current track to the next track. Adds the old track
    /// to prev tracks and fills up the next tracks from the current context
    pub fn next_track(&mut self) -> Result<Option<u32>, Error> {
        // when we skip in repeat track, we don't repeat the current track anymore
        if self.repeat_track() {
            self.set_repeat_track(false);
        }

        let old_track = self.player_mut().track.take();

        if let Some(old_track) = old_track {
            // only add songs from our context to our previous tracks
            if old_track.is_context() || old_track.is_autoplay() {
                self.push_prev(old_track)
            }
        }

        let new_track = loop {
            match self.get_next_track() {
                Some(next) if next.uid.starts_with(IDENTIFIER_DELIMITER) => {
                    self.push_prev(next);
                    continue;
                }
                Some(next) if next.is_unavailable() => continue,
                other => break other,
            };
        };

        let new_track = match new_track {
            None => return Ok(None),
            Some(t) => t,
        };

        self.fill_up_next_tracks()?;

        let update_index = if new_track.is_queue() {
            None
        } else if new_track.is_autoplay() {
            self.set_active_context(ContextType::Autoplay);
            None
        } else {
            match new_track.get_context_index() {
                Some(new_index) => Some(new_index as u32),
                None => {
                    error!("the given context track had no set context_index");
                    None
                }
            }
        };

        if let Some(update_index) = update_index {
            self.update_current_index(|i| i.track = update_index)
        } else {
            self.player_mut().index.clear()
        }

        self.set_track(new_track);
        self.update_restrictions();

        Ok(Some(self.player().index.track))
    }

    /// Move to the prev track
    ///
    /// Updates the current track to the prev track. Adds the old track
    /// to next tracks (when from the context) and fills up the prev tracks from the
    /// current context
    pub fn prev_track(&mut self) -> Result<Option<&MessageField<ProvidedTrack>>, Error> {
        let old_track = self.player_mut().track.take();

        if let Some(old_track) = old_track {
            if old_track.is_context() || old_track.is_autoplay() {
                // todo: O(n)
                self.next_tracks_mut().insert(0, old_track);
            }
        }

        // handle possible delimiter
        if matches!(self.prev_tracks().last(), Some(prev) if prev.uid.starts_with(IDENTIFIER_DELIMITER))
        {
            let delimiter = self
                .prev_tracks_mut()
                .pop()
                .expect("item that was prechecked");

            let next_tracks = self.next_tracks_mut();
            if next_tracks.len() >= SPOTIFY_MAX_NEXT_TRACKS_SIZE {
                let _ = next_tracks.pop();
            }
            // todo: O(n)
            next_tracks.insert(0, delimiter)
        }

        while self.next_tracks().len() > SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            let _ = self.next_tracks_mut().pop();
        }

        let new_track = match self.prev_tracks_mut().pop() {
            None => return Ok(None),
            Some(t) => t,
        };

        if matches!(self.active_context, ContextType::Autoplay if new_track.is_context()) {
            // transition back to default context
            self.set_active_context(ContextType::Default);
        }

        self.fill_up_next_tracks()?;
        self.set_track(new_track);

        if self.player().index.track == 0 {
            warn!("prev: trying to skip into negative, index update skipped")
        } else {
            self.update_current_index(|i| i.track -= 1)
        }

        self.update_restrictions();

        Ok(Some(self.current_track(|t| t)))
    }

    pub fn current_track<F: Fn(&'ct MessageField<ProvidedTrack>) -> R, R>(
        &'ct self,
        access: F,
    ) -> R {
        access(&self.player().track)
    }

    pub fn set_track(&mut self, track: ProvidedTrack) {
        self.player_mut().track = MessageField::some(track)
    }

    pub fn set_next_tracks(&mut self, mut tracks: Vec<ProvidedTrack>) {
        // mobile only sends a set_queue command instead of an add_to_queue command
        // in addition to handling the mobile add_to_queue handling, this should also handle
        // a mass queue addition
        tracks
            .iter_mut()
            .filter(|t| t.is_from_queue())
            .for_each(|t| {
                t.set_provider(Provider::Queue);
                // technically we could preserve the queue-uid here,
                // but it seems to work without that, so we just override it
                t.uid = format!("q{}", self.queue_count);
                self.queue_count += 1;
            });

        // when you drag 'n drop the current track in the queue view into the "Next from: ..."
        // section, it is only send as an empty item with just the provider and metadata, so we have
        // to provide set the uri from the current track manually
        tracks
            .iter_mut()
            .filter(|t| t.uri.is_empty())
            .for_each(|t| t.uri = self.current_track(|ct| ct.uri.clone()));

        self.player_mut().next_tracks = tracks;
    }

    pub fn set_prev_tracks(&mut self, tracks: Vec<ProvidedTrack>) {
        self.player_mut().prev_tracks = tracks;
    }

    pub fn clear_prev_track(&mut self) {
        self.prev_tracks_mut().clear()
    }

    /// Remove all queued tracks
    ///
    /// Keeps the tracks from the context in the next tracks and fills
    /// them up from the current context afterwards
    pub fn clear_queue(&mut self) -> Result<(), Error> {
        self.next_tracks_mut().retain(|t| !t.is_queue());
        self.fill_up_next_tracks()?;
        self.update_queue_revision();
        Ok(())
    }

    pub fn clear_next_tracks(&mut self) {
        // respect queued track and don't throw them out of our next played tracks
        let first_non_queued_track = self
            .next_tracks()
            .iter()
            .enumerate()
            .find(|(_, track)| !track.is_queue());

        if let Some((non_queued_track, _)) = first_non_queued_track {
            while self.next_tracks().len() > non_queued_track
                && self.next_tracks_mut().pop().is_some()
            {}
        }
    }

    pub fn fill_up_next_tracks(&mut self) -> Result<(), Error> {
        let ctx = self.get_context(self.fill_up_context)?;
        let mut new_index = ctx.index.track as usize;
        let mut iteration = ctx.index.page;

        while self.next_tracks().len() < SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            let ctx = self.get_context(self.fill_up_context)?;
            let track = match ctx.tracks.get(new_index) {
                None if self.repeat_context() => {
                    let delimiter = Self::new_delimiter(iteration.into());
                    iteration += 1;
                    new_index = 0;
                    delimiter
                }
                None if !matches!(self.fill_up_context, ContextType::Autoplay)
                    && self.autoplay_context.is_some()
                    && !self.repeat_context() =>
                {
                    self.update_context_index(self.fill_up_context, new_index)?;

                    // transition to autoplay as fill up context
                    self.fill_up_context = ContextType::Autoplay;
                    new_index = self.get_context(ContextType::Autoplay)?.index.track as usize;

                    // add delimiter to only display the current context
                    Self::new_delimiter(iteration.into())
                }
                None if self.autoplay_context.is_some() => {
                    match self
                        .get_context(ContextType::Autoplay)?
                        .tracks
                        .get(new_index)
                    {
                        None => break,
                        Some(ct) => {
                            new_index += 1;
                            ct.clone()
                        }
                    }
                }
                None => break,
                Some(ct) if ct.is_unavailable() || self.is_skip_track(ct, Some(iteration)) => {
                    debug!(
                        "skipped track {} during fillup as it's unavailable or should be skipped",
                        ct.uri
                    );
                    new_index += 1;
                    continue;
                }
                Some(ct) => {
                    new_index += 1;
                    ct.clone()
                }
            };

            self.next_tracks_mut().push(track);
        }

        debug!(
            "finished filling up next_tracks ({})",
            self.next_tracks().len()
        );

        self.update_context_index(self.fill_up_context, new_index)?;

        // the web-player needs a revision update, otherwise the queue isn't updated in the ui
        self.update_queue_revision();

        Ok(())
    }

    pub fn preview_next_track(&mut self) -> Option<SpotifyUri> {
        let next = if self.repeat_track() {
            self.current_track(|t| &t.uri)
        } else {
            &self.next_tracks().first()?.uri
        };

        SpotifyUri::from_uri(next).ok()
    }

    pub fn has_next_tracks(&self, min: Option<usize>) -> bool {
        if let Some(min) = min {
            self.next_tracks().len() >= min
        } else {
            !self.next_tracks().is_empty()
        }
    }

    pub fn recent_track_uris(&self) -> Vec<String> {
        let mut prev = self
            .prev_tracks()
            .iter()
            .map(|t| t.uri.clone())
            .collect::<Vec<_>>();

        prev.push(self.current_track(|t| t.uri.clone()));
        prev
    }

    pub fn mark_unavailable(&mut self, id: &SpotifyUri) -> Result<(), Error> {
        let uri = id.to_uri();

        debug!("marking {uri} as unavailable");

        let next_tracks = self.next_tracks_mut();
        while let Some(pos) = next_tracks.iter().position(|t| t.uri == uri) {
            let _ = next_tracks.remove(pos);
        }

        for next_track in next_tracks {
            Self::mark_as_unavailable_for_match(next_track, &uri)
        }

        let prev_tracks = self.prev_tracks_mut();
        while let Some(pos) = prev_tracks.iter().position(|t| t.uri == uri) {
            let _ = prev_tracks.remove(pos);
        }

        for prev_track in prev_tracks {
            Self::mark_as_unavailable_for_match(prev_track, &uri)
        }

        self.unavailable_uri.push(uri);
        self.fill_up_next_tracks()?;
        self.update_queue_revision();

        Ok(())
    }

    /// Remembers the skip of a title the player gave up while the context the skip needs is
    /// not resolved yet. Returns `true` when the skip is owed, `false` when it can run now.
    ///
    /// A transfer loads its title before the context is resolved. Without the context
    /// [mark_unavailable](ConnectState::mark_unavailable) fails and the skip would be lost.
    pub fn owe_skip_until_context(&mut self, id: &SpotifyUri, play_request_id: u64) -> bool {
        if self.get_context(self.fill_up_context).is_ok() {
            return false;
        }

        let uri = id.to_uri();
        debug!("marking {uri} as unavailable is owed until the context is resolved");
        self.owed_skip = Some((play_request_id, uri));
        true
    }

    /// Hands an owed skip out, once, when its context is there. `None` when nothing is owed,
    /// when the context is still missing (the skip stays owed) or when another title is the
    /// current one by now (the skip is dropped).
    pub fn take_owed_skip(&mut self) -> Option<(u64, SpotifyUri)> {
        self.owed_skip.as_ref()?;
        if self.get_context(self.fill_up_context).is_err() {
            return None;
        }

        let (play_request_id, uri) = self.owed_skip.take()?;
        if self.current_track(|t| t.uri != uri) {
            debug!("dropping the owed skip of {uri}, another title is current");
            return None;
        }

        SpotifyUri::from_uri(&uri)
            .ok()
            .map(|id| (play_request_id, id))
    }

    pub fn add_to_queue(&mut self, mut track: ProvidedTrack, rev_update: bool) {
        track.uid = format!("q{}", self.queue_count);
        self.queue_count += 1;

        track.set_provider(Provider::Queue);
        if !track.is_from_queue() {
            track.set_from_queue(true);
        }

        let next_tracks = self.next_tracks_mut();
        if let Some(next_not_queued_track) = next_tracks.iter().position(|t| !t.is_queue()) {
            next_tracks.insert(next_not_queued_track, track);
        } else {
            next_tracks.push(track)
        }

        while next_tracks.len() > SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            next_tracks.pop();
        }

        if rev_update {
            self.update_queue_revision();
        }
        self.update_restrictions();
    }
}

// A transfer loads the current title before its context is resolved (spirc `handle_transfer`).
// A title the player gives up in that window cannot be skipped yet: `mark_unavailable` needs
// the context. These cells drive `ConnectState` the way the `Unavailable` arm of spirc and its
// hook behind the context resolver do. `SpircTask` itself has no harness (it needs a Session
// and a Player), so `title_given_up` and `resolver_finished` restate those two call sites.
#[cfg(test)]
mod owed_skip_tests {
    use crate::{
        core::{SpotifyUri, audio_key::DENIAL_LIMIT},
        protocol::{
            connect::{Device, DeviceInfo},
            player::{ContextIndex, PlayerState, ProvidedTrack},
            transfer_state::TransferState,
        },
        spirc::{UnavailableAction, unavailable_action},
        state::{
            ConnectState,
            context::{ResetContext, StateContext},
            provider::{IsProvider, Provider},
        },
    };
    use protobuf::MessageField;
    use std::collections::HashMap;

    #[derive(Debug, PartialEq)]
    enum Step {
        Owed,
        Skipped,
        Paused,
        Nothing,
    }

    fn uri(n: usize) -> String {
        format!("spotify:track:{n:022}")
    }

    fn id(n: usize) -> SpotifyUri {
        SpotifyUri::from_uri(&uri(n)).unwrap()
    }

    fn track(n: usize) -> ProvidedTrack {
        let mut t = ProvidedTrack {
            uri: uri(n),
            uid: format!("uid{n}"),
            ..Default::default()
        };
        t.set_provider(Provider::Context);
        t
    }

    fn current(state: &ConnectState) -> String {
        state.current_track(|t| t.uri.clone())
    }

    // the state right after `handle_transfer`: active, current title set, no context yet
    fn state_after_transfer() -> ConnectState {
        let mut state = ConnectState::default();
        state.request.device = MessageField::some(Device {
            device_info: MessageField::some(DeviceInfo::default()),
            player_state: MessageField::some(PlayerState::default()),
            ..Default::default()
        });
        state.reset();
        state.set_active(true);
        state.set_track(track(0));
        state
    }

    // the resolver hands the album in and `try_finish` completes the transfer
    fn the_context_arrives(state: &mut ConnectState, titles: usize) {
        state.context = Some(StateContext {
            tracks: (0..titles).map(track).collect::<Vec<_>>().into(),
            metadata: HashMap::new(),
            restrictions: None,
            index: ContextIndex::new(),
        });
        state.finish_transfer(TransferState::default()).unwrap();
    }

    // the `Unavailable` arm of spirc for `id`, with `denials` refused titles in a row so far
    fn title_given_up(
        state: &mut ConnectState,
        id: &SpotifyUri,
        play_request_id: u64,
        denials: u32,
    ) -> Step {
        match unavailable_action(denials, DENIAL_LIMIT) {
            UnavailableAction::PauseKeepQueue => Step::Paused,
            UnavailableAction::SkipNext => {
                if state.owe_skip_until_context(id, play_request_id) {
                    return Step::Owed;
                }
                state
                    .mark_unavailable(id)
                    .unwrap_or_else(|why| panic!("the skip failed: {why}"));
                if current(state) == id.to_uri() {
                    state.next_track().unwrap();
                }
                Step::Skipped
            }
        }
    }

    // the current title is given up
    fn give_up(state: &mut ConnectState, play_request_id: u64, denials: u32) -> Step {
        let id = SpotifyUri::from_uri(&current(state)).unwrap();
        title_given_up(state, &id, play_request_id, denials)
    }

    // the hook of spirc behind `try_finish`: an owed skip is dispatched again
    fn resolver_finished(state: &mut ConnectState, denials: u32) -> Step {
        match state.take_owed_skip() {
            None => Step::Nothing,
            Some((play_request_id, id)) => title_given_up(state, &id, play_request_id, denials),
        }
    }

    #[test]
    fn a_title_given_up_before_the_context_ends_on_title_two_once_the_context_is_there() {
        let mut state = state_after_transfer();

        // nothing fails and nothing moves while the context is missing
        assert_eq!(give_up(&mut state, 7, 1), Step::Owed);
        assert_eq!(current(&state), uri(0));

        the_context_arrives(&mut state, 13);
        assert_eq!(current(&state), uri(0));
        assert_eq!(state.next_tracks().len(), 12);

        assert_eq!(resolver_finished(&mut state, 1), Step::Skipped);
        assert_eq!(current(&state), uri(1));
        assert_eq!(state.next_tracks().len(), 11);
        assert_eq!(state.unavailable_uri, vec![uri(0)]);
    }

    #[test]
    fn an_owed_skip_is_handed_out_once_with_its_play_request() {
        let mut state = state_after_transfer();
        assert!(state.owe_skip_until_context(&id(0), 7));
        the_context_arrives(&mut state, 13);

        assert_eq!(state.take_owed_skip(), Some((7, id(0))));
        // a resolver that finishes a second time (more pages, autoplay) finds nothing
        assert_eq!(state.take_owed_skip(), None);
        assert_eq!(current(&state), uri(0));
    }

    #[test]
    fn an_owed_skip_stays_owed_while_the_context_is_missing() {
        let mut state = state_after_transfer();
        assert!(state.owe_skip_until_context(&id(0), 7));

        assert_eq!(state.take_owed_skip(), None);

        the_context_arrives(&mut state, 13);
        assert_eq!(state.take_owed_skip(), Some((7, id(0))));
    }

    #[test]
    fn a_later_give_up_replaces_the_owed_skip() {
        let mut state = state_after_transfer();
        assert!(state.owe_skip_until_context(&id(0), 7));

        // another title was loaded and given up as well, still without a context
        state.set_track(track(3));
        assert!(state.owe_skip_until_context(&id(3), 8));

        the_context_arrives(&mut state, 13);
        assert_eq!(state.take_owed_skip(), Some((8, id(3))));
        assert_eq!(state.take_owed_skip(), None);
    }

    #[test]
    fn an_owed_skip_is_dropped_when_another_title_is_current() {
        let mut state = state_after_transfer();
        assert!(state.owe_skip_until_context(&id(0), 7));

        // the user picked another title before the context arrived
        state.set_track(track(3));
        the_context_arrives(&mut state, 13);

        assert_eq!(state.take_owed_skip(), None);
        assert_eq!(current(&state), uri(3));
        // dropped, not kept: when title 1 is current again later it is not skipped
        state.set_track(track(0));
        assert_eq!(state.take_owed_skip(), None);
        assert_eq!(current(&state), uri(0));
    }

    #[test]
    fn an_owed_skip_does_not_carry_over_into_a_new_context_request() {
        // a new transfer or play command resets the context first, whether it names another
        // context, the same one, or none (which is also what going inactive does)
        for reset_as in [
            ResetContext::WhenDifferent("spotify:album:another"),
            ResetContext::WhenDifferent(""),
            ResetContext::Completely,
        ] {
            let mut state = state_after_transfer();
            assert!(state.owe_skip_until_context(&id(0), 7));

            state.reset_context(reset_as);
            state.set_track(track(0));
            the_context_arrives(&mut state, 13);

            assert_eq!(state.take_owed_skip(), None);
            assert_eq!(current(&state), uri(0));
        }
    }

    #[test]
    fn an_owed_skip_outlives_a_reset_of_the_context_index() {
        // shuffle and repeat changes reset the index only, the skip is still due
        let mut state = state_after_transfer();
        assert!(state.owe_skip_until_context(&id(0), 7));

        state.reset_context(ResetContext::DefaultIndex);
        the_context_arrives(&mut state, 13);

        assert_eq!(state.take_owed_skip(), Some((7, id(0))));
    }

    // The count of refused titles lives in core (`KeyPacer::note_outcome`, one per distinct
    // refused title). Every title that becomes current here is loaded once and refused once,
    // so the count is the number of titles loaded so far.
    #[test]
    fn five_refused_titles_the_first_before_the_context_pause_on_the_fifth() {
        let mut state = state_after_transfer();
        let mut loaded = vec![current(&state)];

        // title 1 is given up before the context of the transfer is there
        assert_eq!(give_up(&mut state, 1, 1), Step::Owed);
        the_context_arrives(&mut state, 13);
        assert_eq!(resolver_finished(&mut state, 1), Step::Skipped);
        loaded.push(current(&state));

        // titles 2 to 4 are given up with the context in place
        for play_request_id in 2..=4 {
            let denials = loaded.len() as u32;
            assert_eq!(give_up(&mut state, play_request_id, denials), Step::Skipped);
            assert_eq!(resolver_finished(&mut state, denials), Step::Nothing);
            loaded.push(current(&state));
        }

        // every refused title was current once, in order, so title 5 is the fifth refusal
        assert_eq!(loaded, (0..5).map(uri).collect::<Vec<_>>());
        assert_eq!(give_up(&mut state, 5, loaded.len() as u32), Step::Paused);
        assert_eq!(current(&state), uri(4));
        assert_eq!(state.next_tracks().len(), 8);
        assert_eq!(resolver_finished(&mut state, 5), Step::Nothing);
    }

    #[test]
    fn control_marking_without_a_context_fails_as_before() {
        let mut state = state_after_transfer();

        let err = state.mark_unavailable(&id(0)).unwrap_err();

        assert!(
            err.to_string()
                .contains("context is not available. type: Default"),
            "got: {err}"
        );
        assert_eq!(current(&state), uri(0));
    }

    #[test]
    fn control_a_title_given_up_after_the_context_skips_at_once_and_owes_nothing() {
        let mut state = state_after_transfer();
        the_context_arrives(&mut state, 13);

        assert_eq!(give_up(&mut state, 7, 1), Step::Skipped);

        assert_eq!(current(&state), uri(1));
        assert_eq!(state.next_tracks().len(), 11);
        assert_eq!(state.take_owed_skip(), None);
    }

    #[test]
    fn control_a_context_arriving_with_nothing_owed_moves_nothing() {
        let mut state = state_after_transfer();
        the_context_arrives(&mut state, 13);

        assert_eq!(resolver_finished(&mut state, 0), Step::Nothing);

        assert_eq!(current(&state), uri(0));
        assert_eq!(state.next_tracks().len(), 12);
        assert!(state.unavailable_uri.is_empty());
    }
}
