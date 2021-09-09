//! ## Per-Layer Filtering
//!
//! Per-layer filters permit individual `Layer`s to have their own filter
//! configurations without interfering with other `Layer`s.
//!
//! This module is not public; the public APIs defined in this module are
//! re-exported in the top-level `filter` module. Therefore, this documentation
//! primarily concerns the internal implementation details. For the user-facing
//! public API documentation, see the individual public types in this module, as
//! well as the, see the `Layer` trait documentation's [per-layer filtering
//! section]][1].
//!
//! ## How does per-layer filtering work?
//!
//! As described in the API documentation, the [`Filter`] trait defines a
//! filtering strategy for a per-layer filter. We expect there will be a variety
//! of implementations of [`Filter`], both in `tracing-subscriber` and in user
//! code.
//!
//! To actually *use* a [`Filter`] implementation, it is combined with a
//! [`Layer`] by the [`Filtered`] struct defined in this module. [`Filtered`]
//! implements [`Layer`] by calling into the wrapped [`Layer`], or not, based on
//! the filtering strategy. While there will be a variety of types that implement
//! [`Filter`], all actual *uses* of per-layer filtering will occur through the
//! [`Filtered`] struct. Therefore, most of the implementation details live
//! there.
//!
//! [1]: crate::layer#per-layer-filtering
//! [`Filter`]: crate::layer::Filter
use crate::{
    filter::LevelFilter,
    layer::{self, Context, Layer},
    registry,
};
use std::{
    any::{type_name, TypeId},
    cell::{Cell, RefCell},
    fmt,
    marker::PhantomData,
    sync::Arc,
    thread_local,
};
use tracing_core::{
    span,
    subscriber::{Interest, Subscriber},
    Event, Metadata,
};

/// A [`Layer`] that wraps an inner [`Layer`] and adds a [`Filter`] which
/// controls what spans and events are enabled for that layer.
///
/// This is returned by the [`Layer::with_filter`] method. See the
/// [documentation on per-layer filtering][plf] for details.
///
/// [`Filter`]: crate::layer::Filter
/// [plf]: crate::layer#per-layer-filtering
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
#[derive(Clone)]
pub struct Filtered<L, F, S> {
    filter: F,
    layer: L,
    id: MagicPlfDowncastMarker,
    _s: PhantomData<fn(S)>,
}

/// A per-layer [`Filter`] implemented by a closure or function pointer that
/// determines whether a given span or event is enabled _dynamically_,
/// potentially based on the current [span context].
///
/// See the [documentation on per-layer filtering][plf] for details.
///
/// [span context]: crate::layer::Context
/// [`Filter`]: crate::layer::Filter
/// [plf]: crate::layer#per-layer-filtering
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
pub struct DynFilterFn<
    S,
    // TODO(eliza): should these just be boxed functions?
    F = fn(&Metadata<'_>, &Context<'_, S>) -> bool,
    R = fn(&'static Metadata<'static>) -> Interest,
> {
    enabled: F,
    register_callsite: Option<R>,
    max_level_hint: Option<LevelFilter>,
    _s: PhantomData<fn(S)>,
}

/// A per-layer [`Filter`] implemented by a closure or function pointer that
/// determines whether a given span or event is enabled, based on its
/// [`Metadata`].
///
/// See the [documentation on per-layer filtering][plf] for details.
///
/// [`Metadata`]: tracing_core::Metadata
/// [`Filter`]: crate::layer::Filter
/// [plf]: crate::layer#per-layer-filtering
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
#[derive(Clone)]
pub struct FilterFn<F = fn(&Metadata<'_>) -> bool> {
    enabled: F,
    max_level_hint: Option<LevelFilter>,
}

/// Uniquely identifies an individual [`Filter`] instance in the context of
/// a [`Subscriber`].
///
/// When adding a [`Filtered`] [`Layer`] to a [`Subscriber`], the [`Subscriber`]
/// generates a `FilterId` for that [`Filtered`] layer. The [`Filtered`] layer
/// will then use the generated ID to query whether a particular span was
/// previously enabled by that layer's [`Filter`].
///
/// **Note**: Currently, the [`Registry`] type provided by this crate is the
/// **only** [`Subscriber`] implementation capable of participating in per-layer
/// filtering. Therefore, the `FilterId` type cannot currently be constructed by
/// code outside of `tracing-subscriber`. In the future, new APIs will be added to `tracing-subscriber` to
/// allow non-Registry [`Subscriber`]s to also participate in per-layer
/// filtering. When those APIs are added, subscribers will be responsible
/// for generating and assigning `FilterId`s.
///
/// [`Filter`]: crate::layer::Filter
/// [`Subscriber`]: tracing_core::Subscriber
/// [`Layer`]: crate::layer::Layer
/// [`Registry`]: crate::registry::Registry
#[cfg(feature = "registry")]
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
#[derive(Copy, Clone)]
pub struct FilterId(u64);

/// A bitmap tracking which [`FilterId`]s have enabled a given span or
/// event.
///
/// This is currently a private type that's used exclusively by the
/// [`Registry`]. However, in the future, this may become a public API, in order
/// to allow user subscribers to host [`Filter`]s.
///
/// [`Registry`]: crate::Registry
/// [`Filter`]: crate::layer::Filter
#[derive(Default, Copy, Clone, Eq, PartialEq)]
pub(crate) struct FilterMap {
    bits: u64,
}

/// The current state of `enabled` calls to per-layer filters on this
/// thread.
///
/// When `Filtered::enabled` is called, the filter will set the bit
/// corresponding to its ID if the filter will disable the event/span being
/// filtered. When the event or span is recorded, the per-layer filter will
/// check its bit to determine if it disabled that event or span, and skip
/// forwarding the event or span to the inner layer if the bit is set. Once
/// a span or event has been skipped by a per-layer filter, it unsets its
/// bit, so that the `FilterMap` has been cleared for the next set of
/// `enabled` calls.
///
/// FilterState is also read by the `Registry`, for two reasons:
///
/// 1. When filtering a span, the Registry must store the `FilterMap`
///    generated by `Filtered::enabled` calls for that span as part of the
///    span's per-span data. This allows `Filtered` layers to determine
///    whether they had previously disabled a given span, and avoid showing it
///    to the wrapped layer if it was disabled.
///
///    This allows `Filtered` layers to also filter out the spans they
///    disable from span traversals (such as iterating over parents, etc).
/// 2. If all the bits are set, then every per-layer filter has decided it
///    doesn't want to enable that span or event. In that case, the
///    `Registry`'s `enabled` method will return `false`, so that
///     recording a span or event can be skipped entirely.
#[derive(Debug)]
pub(crate) struct FilterState {
    enabled: Cell<FilterMap>,
    // TODO(eliza): `Interest`s should _probably_ be `Copy`. The only reason
    // they're not is our Obsessive Commitment to Forwards-Compatibility. If
    // this changes in tracing-core`, we can make this a `Cell` rather than
    // `RefCell`...
    interest: RefCell<Option<Interest>>,

    #[cfg(debug_assertions)]
    counters: DebugCounters,
}

/// Extra counters added to `FilterState` used only to make debug assertions.
#[cfg(debug_assertions)]
#[derive(Debug, Default)]
struct DebugCounters {
    /// How many per-layer filters have participated in the current `enabled`
    /// call?
    in_filter_pass: Cell<usize>,

    /// How many per-layer filters have participated in the current `register_callsite`
    /// call?
    in_interest_pass: Cell<usize>,
}

thread_local! {
    pub(crate) static FILTERING: FilterState = FilterState::new();
}

// === impl Filter ===
#[cfg(feature = "registry")]
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
impl<S> layer::Filter<S> for LevelFilter {
    fn enabled(&self, meta: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        meta.level() <= self
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        if meta.level() <= self {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(*self)
    }
}

impl<S> layer::Filter<S> for Arc<dyn layer::Filter<S> + Send + Sync + 'static> {
    #[inline]
    fn enabled(&self, meta: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
        (**self).enabled(meta, cx)
    }

    #[inline]
    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        (**self).callsite_enabled(meta)
    }

    #[inline]
    fn max_level_hint(&self) -> Option<LevelFilter> {
        (**self).max_level_hint()
    }
}

impl<S> layer::Filter<S> for Box<dyn layer::Filter<S> + Send + Sync + 'static> {
    #[inline]
    fn enabled(&self, meta: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
        (**self).enabled(meta, cx)
    }

    #[inline]
    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        (**self).callsite_enabled(meta)
    }

    #[inline]
    fn max_level_hint(&self) -> Option<LevelFilter> {
        (**self).max_level_hint()
    }
}

// === impl Filtered ===

impl<L, F, S> Filtered<L, F, S> {
    /// Wraps the provided [`Layer`] so that it is filtered by the given
    /// [`Filter`].
    ///
    /// This is equivalent to calling the [`Layer::with_filter`] method.
    ///
    /// See the [documentation on per-layer filtering][plf] for details.
    ///
    /// [`Filter`]: crate::layer::Filter
    /// [plf]: crate::layer#per-layer-filtering
    pub fn new(layer: L, filter: F) -> Self {
        Self {
            layer,
            filter,
            id: MagicPlfDowncastMarker(FilterId::disabled()),
            _s: PhantomData,
        }
    }

    #[inline(always)]
    fn id(&self) -> FilterId {
        self.id.0
    }

    fn did_enable(&self, f: impl FnOnce()) {
        FILTERING.with(|filtering| filtering.did_enable(self.id(), f))
    }
}

impl<S, L, F> Layer<S> for Filtered<L, F, S>
where
    S: Subscriber + for<'span> registry::LookupSpan<'span> + 'static,
    F: layer::Filter<S> + 'static,
    L: Layer<S>,
{
    fn on_layer(&mut self, subscriber: &mut S) {
        self.id = MagicPlfDowncastMarker(subscriber.register_filter());
        self.layer.on_layer(subscriber);
    }

    // TODO(eliza): can we figure out a nice way to make the `Filtered` layer
    // not call `is_enabled_for` in hooks that the inner layer doesn't actually
    // have real implementations of? probably not...
    //
    // it would be cool if there was some wild rust reflection way of checking
    // if a trait impl has the default impl of a trait method or not, but that's
    // almsot certainly impossible...right?

    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        let interest = self.filter.callsite_enabled(metadata);

        // If the filter didn't disable the callsite, allow the inner layer to
        // register it — since `register_callsite` is also used for purposes
        // such as reserving/caching per-callsite data, we want the inner layer
        // to be able to perform any other registration steps. However, we'll
        // ignore its `Interest`.
        if !interest.is_never() {
            self.layer.register_callsite(metadata);
        }

        // Add our `Interest` to the current sum of per-layer filter `Interest`s
        // for this callsite.
        FILTERING.with(|filtering| filtering.add_interest(interest));

        // don't short circuit! if the stack consists entirely of `Layer`s with
        // per-layer filters, the `Registry` will return the actual `Interest`
        // value that's the sum of all the `register_callsite` calls to those
        // per-layer filters. if we returned an actual `never` interest here, a
        // `Layered` layer would short-circuit and not allow any `Filtered`
        // layers below us if _they_ are interested in the callsite.
        Interest::always()
    }

    fn enabled(&self, metadata: &Metadata<'_>, cx: Context<'_, S>) -> bool {
        let cx = cx.with_filter(self.id());
        let enabled = self.filter.enabled(metadata, &cx);
        FILTERING.with(|filtering| filtering.set(self.id(), enabled));

        if enabled {
            // If the filter enabled this metadata, ask the wrapped layer if
            // _it_ wants it --- it might have a global filter.
            self.layer.enabled(metadata, cx)
        } else {
            // Otherwise, return `true`. The _per-layer_ filter disabled this
            // metadata, but returning `false` in `Layer::enabled` will
            // short-circuit and globally disable the span or event. This is
            // *not* what we want for per-layer filters, as other layers may
            // still want this event. Returning `true` here means we'll continue
            // asking the next layer in the stack.
            //
            // Once all per-layer filters have been evaluated, the `Registry`
            // at the root of the stack will return `false` from its `enabled`
            // method if *every* per-layer  filter disabled this metadata.
            // Otherwise, the individual per-layer filters will skip the next
            // `new_span` or `on_event` call for their layer if *they* disabled
            // the span or event, but it was not globally disabled.
            true
        }
    }

    fn new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, cx: Context<'_, S>) {
        self.did_enable(|| {
            self.layer.new_span(attrs, id, cx.with_filter(self.id()));
        })
    }

    #[doc(hidden)]
    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.filter.max_level_hint()
    }

    fn on_record(&self, span: &span::Id, values: &span::Record<'_>, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(span, self.id()) {
            self.layer.on_record(span, values, cx)
        }
    }

    fn on_follows_from(&self, span: &span::Id, follows: &span::Id, cx: Context<'_, S>) {
        // only call `on_follows_from` if both spans are enabled by us
        if cx.is_enabled_for(span, self.id()) && cx.is_enabled_for(follows, self.id()) {
            self.layer
                .on_follows_from(span, follows, cx.with_filter(self.id()))
        }
    }

    fn on_event(&self, event: &Event<'_>, cx: Context<'_, S>) {
        self.did_enable(|| {
            self.layer.on_event(event, cx.with_filter(self.id()));
        })
    }

    fn on_enter(&self, id: &span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(id, self.id()) {
            self.layer.on_enter(id, cx)
        }
    }

    fn on_exit(&self, id: &span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(id, self.id()) {
            self.layer.on_exit(id, cx)
        }
    }

    fn on_close(&self, id: span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(&id, self.id()) {
            self.layer.on_close(id, cx)
        }
    }

    // XXX(eliza): the existence of this method still makes me sad...
    fn on_id_change(&self, old: &span::Id, new: &span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(old, self.id()) {
            self.layer.on_id_change(old, new, cx)
        }
    }

    #[doc(hidden)]
    #[inline]
    unsafe fn downcast_raw(&self, id: TypeId) -> Option<*const ()> {
        match id {
            id if id == TypeId::of::<Self>() => Some(self as *const _ as *const ()),
            id if id == TypeId::of::<L>() => Some(&self.layer as *const _ as *const ()),
            id if id == TypeId::of::<F>() => Some(&self.filter as *const _ as *const ()),
            id if id == TypeId::of::<MagicPlfDowncastMarker>() => {
                Some(&self.id as *const _ as *const ())
            }
            _ => None,
        }
    }
}

impl<F, L, S> fmt::Debug for Filtered<F, L, S>
where
    F: fmt::Debug,
    L: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Filtered")
            .field("filter", &self.filter)
            .field("layer", &self.layer)
            .field("id", &self.id)
            .finish()
    }
}

// === impl FilterFn ===

/// Constructs a [`FilterFn`], which implements the [`Filter`] trait, from
/// a function or closure that returns `true` if a span or event should be enabled.
///
/// This is equivalent to calling [`FilterFn::new`].
///
/// See the [documentation on per-layer filtering][plf] for details on using
/// [`Filter`]s.
///
/// [`Filter`]: crate::layer::Filter
/// [plf]: crate::layer#per-layer-filtering
///
/// # Examples
///
/// ```
/// use tracing_subscriber::{
///     layer::{Layer, SubscriberExt},
///     filter,
///     util::SubscriberInitExt,
/// };
///
/// let my_filter = filter::filter_fn(|metadata| {
///     // Only enable spans or events with the target "interesting_things"
///     metadata.target() == "interesting_things"
/// });
///
/// let my_layer = tracing_subscriber::fmt::layer();
///
/// tracing_subscriber::registry()
///     .with(my_layer.with_filter(my_filter))
///     .init();
///
/// // This event will not be enabled.
/// tracing::warn!("something important but uninteresting happened!");
///
/// // This event will be enabled.
/// tracing::debug!(target: "interesting_things", "an interesting minor detail...");
/// ```
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
pub fn filter_fn<F>(f: F) -> FilterFn<F>
where
    F: Fn(&Metadata<'_>) -> bool,
{
    FilterFn::new(f)
}

/// Constructs a [`DynFilterFn`], which implements the [`Filter`] trait, from
/// a function or closure that returns `true` if a span or event should be
/// enabled within a particular [span context][`Context`].
///
/// This is equivalent to calling [`DynFilterFn::new`].
///
/// Unlike [`filter_fn`], this function takes a closure or function pointer
/// taking the [`Metadata`] for a span or event *and* the current [`Context`].
/// This means that a [`DynFilterFn`] can choose whether to enable spans or
/// events based on information about the _current_ span (or its parents).
///
/// If this is *not* necessary, use [`filter_fn`] instead.
///
/// See the [documentation on per-layer filtering][plf] for details on using
/// [`Filter`]s.
///
/// # Examples
///
/// ```
/// use tracing_subscriber::{
///     layer::{Layer, SubscriberExt},
///     filter,
///     util::SubscriberInitExt,
/// };
///
/// // Only enable spans or events within a span named "interesting_span".
/// let my_filter = filter::dynamic_filter_fn(|metadata, cx| {
///     // If this *is* "interesting_span", make sure to enable it.
///     if metadata.is_span() && metadata.name() == "interesting_span" {
///         return true;
///     }
///
///     // Otherwise, are we in an interesting span?
///     if let Some(current_span) = cx.lookup_current() {
///         return current_span.name() == "interesting_span";
///     }
///
///     false
/// });
///
/// let my_layer = tracing_subscriber::fmt::layer();
///
/// tracing_subscriber::registry()
///     .with(my_layer.with_filter(my_filter))
///     .init();
///
/// // This event will not be enabled.
/// tracing::info!("something happened");
///
/// tracing::info_span!("interesting_span").in_scope(|| {
///     // This event will be enabled.
///     tracing::debug!("something else happened");
/// });
/// ```
///
/// [`Filter`]: crate::layer::Filter
/// [plf]: crate::layer#per-layer-filtering
/// [`Context`]: crate::layer::Context
/// [`Metadata`]: tracing_core::Metadata
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
pub fn dynamic_filter_fn<S, F>(f: F) -> DynFilterFn<S, F>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
{
    DynFilterFn::new(f)
}

impl<F> FilterFn<F>
where
    F: Fn(&Metadata<'_>) -> bool,
{
    /// Constructs a [`Filter`] from a function or closure that returns `true`
    /// if a span or event should be enabled, based on its [`Metadata`].
    ///
    /// If determining whether a span or event should be enabled also requires
    /// information about the current span context, use [`DynFilterFn`] instead.
    ///
    /// See the [documentation on per-layer filtering][plf] for details on using
    /// [`Filter`]s.
    ///
    /// [`Filter`]: crate::layer::Filter
    /// [plf]: crate::layer#per-layer-filtering
    /// [`Metadata`]: tracing_core::Metadata
    ///
    /// # Examples
    ///
    /// ```
    /// use tracing_subscriber::{
    ///     layer::{Layer, SubscriberExt},
    ///     filter::FilterFn,
    ///     util::SubscriberInitExt,
    /// };
    ///
    /// let my_filter = FilterFn::new(|metadata| {
    ///     // Only enable spans or events with the target "interesting_things"
    ///     metadata.target() == "interesting_things"
    /// });
    ///
    /// let my_layer = tracing_subscriber::fmt::layer();
    ///
    /// tracing_subscriber::registry()
    ///     .with(my_layer.with_filter(my_filter))
    ///     .init();
    ///
    /// // This event will not be enabled.
    /// tracing::warn!("something important but uninteresting happened!");
    ///
    /// // This event will be enabled.
    /// tracing::debug!(target: "interesting_things", "an interesting minor detail...");
    /// ```
    pub fn new(enabled: F) -> Self {
        Self {
            enabled,
            max_level_hint: None,
        }
    }

    /// Sets the highest verbosity [`Level`] the filter function will enable.
    ///
    /// The value passed to this method will be returned by this `FilterFn`'s
    /// [`Filter::max_level_hint`] method.
    ///
    /// If the provided function will not enable all levels, it is recommended
    /// to call this method to configure it with the most verbose level it will
    /// enable.
    ///
    /// # Examples
    ///
    /// ```
    /// use tracing_subscriber::{
    ///     layer::{Layer, SubscriberExt},
    ///     filter::{filter_fn, LevelFilter},
    ///     util::SubscriberInitExt,
    /// };
    /// use tracing_core::Level;
    ///
    /// let my_filter = filter_fn(|metadata| {
    ///     // Only enable spans or events with targets starting with `my_crate`
    ///     // and levels at or below `INFO`.
    ///     metadata.level() <= &Level::INFO && metadata.target().starts_with("my_crate")
    /// })
    ///     // Since the filter closure will only enable the `INFO` level and
    ///     // below, set the max level hint
    ///     .with_max_level_hint(LevelFilter::INFO);
    ///
    /// let my_layer = tracing_subscriber::fmt::layer();
    ///
    /// tracing_subscriber::registry()
    ///     .with(my_layer.with_filter(my_filter))
    ///     .init();
    /// ```
    ///
    /// [`Level`]: tracing_core::Level
    /// [`Filter::max_level_hint`]: crate::layer::Filter::max_level_hint
    pub fn with_max_level_hint(self, max_level_hint: impl Into<LevelFilter>) -> Self {
        Self {
            max_level_hint: Some(max_level_hint.into()),
            ..self
        }
    }

    fn is_below_max_level(&self, metadata: &Metadata<'_>) -> bool {
        self.max_level_hint
            .as_ref()
            .map(|hint| metadata.level() <= hint)
            .unwrap_or(true)
    }
}

impl<S, F> layer::Filter<S> for FilterFn<F>
where
    F: Fn(&Metadata<'_>) -> bool,
{
    fn enabled(&self, metadata: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        let enabled = (self.enabled)(metadata);
        debug_assert!(
            !enabled || self.is_below_max_level(metadata),
            "FilterFn<{}> claimed it would only enable {:?} and below, \
            but it enabled metadata with the {:?} level\nmetadata={:#?}",
            type_name::<F>(),
            self.max_level_hint.unwrap(),
            metadata.level(),
            metadata,
        );

        enabled
    }

    fn callsite_enabled(&self, metadata: &'static Metadata<'static>) -> Interest {
        // Because `self.enabled` takes a `Metadata` only (and no `Context`
        // parameter), we can reasonably assume its results are cachable, and
        // just return `Interest::always`/`Interest::never`.
        if (self.enabled)(metadata) {
            debug_assert!(
                self.is_below_max_level(metadata),
                "FilterFn<{}> claimed it was only interested in {:?} and below, \
                but it enabled metadata with the {:?} level\nmetadata={:#?}",
                type_name::<F>(),
                self.max_level_hint.unwrap(),
                metadata.level(),
                metadata,
            );
            return Interest::always();
        }

        Interest::never()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.max_level_hint
    }
}

impl<F> From<F> for FilterFn<F>
where
    F: Fn(&Metadata<'_>) -> bool,
{
    fn from(enabled: F) -> Self {
        Self::new(enabled)
    }
}

impl<F> fmt::Debug for FilterFn<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilterFn")
            .field("enabled", &format_args!("{}", type_name::<F>()))
            .field("max_level_hint", &self.max_level_hint)
            .finish()
    }
}

// === impl DynFilterFn ==

impl<S, F> DynFilterFn<S, F>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
{
    /// Constructs a [`Filter`] from a function or closure that returns `true`
    /// if a span or event should be enabled in the current [span
    /// context][`Context`].
    ///
    /// Unlike [`FilterFn`], a `DynFilterFn` is constructed from a closure or
    /// function pointer that takes both the [`Metadata`] for a span or event
    /// *and* the current [`Context`]. This means that a [`DynFilterFn`] can
    /// choose whether to enable spans or events based on information about the
    /// _current_ span (or its parents).
    ///
    /// If this is *not* necessary, use [`FilterFn`] instead.
    ///
    /// See the [documentation on per-layer filtering][plf] for details on using
    /// [`Filter`]s.
    ///
    /// [`Filter`]: crate::layer::Filter
    /// [plf]: crate::layer#per-layer-filtering
    /// [`Context`]: crate::layer::Context
    /// [`Metadata`]: tracing_core::Metadata
    ///
    /// # Examples
    ///
    /// ```
    /// use tracing_subscriber::{
    ///     layer::{Layer, SubscriberExt},
    ///     filter::DynFilterFn,
    ///     util::SubscriberInitExt,
    /// };
    ///
    /// // Only enable spans or events within a span named "interesting_span".
    /// let my_filter = DynFilterFn::new(|metadata, cx| {
    ///     // If this *is* "interesting_span", make sure to enable it.
    ///     if metadata.is_span() && metadata.name() == "interesting_span" {
    ///         return true;
    ///     }
    ///
    ///     // Otherwise, are we in an interesting span?
    ///     if let Some(current_span) = cx.lookup_current() {
    ///         return current_span.name() == "interesting_span";
    ///     }
    ///
    ///     false
    /// });
    ///
    /// let my_layer = tracing_subscriber::fmt::layer();
    ///
    /// tracing_subscriber::registry()
    ///     .with(my_layer.with_filter(my_filter))
    ///     .init();
    ///
    /// // This event will not be enabled.
    /// tracing::info!("something happened");
    ///
    /// tracing::info_span!("interesting_span").in_scope(|| {
    ///     // This event will be enabled.
    ///     tracing::debug!("something else happened");
    /// });
    /// ```
    pub fn new(enabled: F) -> Self {
        Self {
            enabled,
            register_callsite: None,
            max_level_hint: None,
            _s: PhantomData,
        }
    }
}

impl<S, F, R> DynFilterFn<S, F, R>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
{
    /// Sets the highest verbosity [`Level`] the filter function will enable.
    ///
    /// The value passed to this method will be returned by this `DynFilterFn`'s
    /// [`Filter::max_level_hint`] method.
    ///
    /// If the provided function will not enable all levels, it is recommended
    /// to call this method to configure it with the most verbose level it will
    /// enable.
    ///
    /// # Examples
    ///
    /// ```
    /// use tracing_subscriber::{
    ///     layer::{Layer, SubscriberExt},
    ///     filter::{DynFilterFn, LevelFilter},
    ///     util::SubscriberInitExt,
    /// };
    /// use tracing_core::Level;
    ///
    /// // Only enable spans or events with levels at or below `INFO`, if
    /// // we are inside a span called "interesting_span".
    /// let my_filter = DynFilterFn::new(|metadata, cx| {
    ///     // If the level is greater than INFO, disable it.
    ///     if metadata.level() > &Level::INFO {
    ///         return false;
    ///     }
    ///
    ///     // If any span in the current scope is named "interesting_span",
    ///     // enable this span or event.
    ///     for span in cx.lookup_current().iter().flat_map(|span| span.scope()) {
    ///         if span.name() == "interesting_span" {
    ///             return true;
    ///          }
    ///     }
    ///
    ///     // Otherwise, disable it.
    ///     false
    /// })
    ///     // Since the filter closure will only enable the `INFO` level and
    ///     // below, set the max level hint
    ///     .with_max_level_hint(LevelFilter::INFO);
    ///
    /// let my_layer = tracing_subscriber::fmt::layer();
    ///
    /// tracing_subscriber::registry()
    ///     .with(my_layer.with_filter(my_filter))
    ///     .init();
    /// ```
    ///
    /// [`Level`]: tracing_core::Level
    /// [`Filter::max_level_hint`]: crate::layer::Filter::max_level_hint
    pub fn with_max_level_hint(self, max_level_hint: impl Into<LevelFilter>) -> Self {
        Self {
            max_level_hint: Some(max_level_hint.into()),
            ..self
        }
    }

    /// Adds a function for filtering callsites to this filter.
    ///
    /// When this filter's [`Filter::callsite_enabled`][cse] method is called,
    /// the provided function will be used rather than the default.
    ///
    /// By default, `DynFilterFn` assumes that, because the filter _may_ depend
    /// dynamically on the current [span context], its result should never be
    /// cached. However, some filtering strategies may require dynamic information
    /// from the current span context in *some* cases, but are able to make
    /// static filtering decisions from [`Metadata`] alone in others.
    ///
    /// For example, consider the filter given in the example for
    /// [`DynFilterFn::new`]. That filter enables all spans named
    /// "interesting_span", and any events and spans that occur inside of an
    /// interesting span. Since the span's name is part of its static
    /// [`Metadata`], the "interesting_span" can be enabled in
    /// [`callsite_enabled`][cse]:
    ///
    /// ```
    /// use tracing_subscriber::{
    ///     layer::{Layer, SubscriberExt},
    ///     filter::DynFilterFn,
    ///     util::SubscriberInitExt,
    /// };
    /// use tracing_core::subscriber::Interest;
    ///
    /// // Only enable spans or events within a span named "interesting_span".
    /// let my_filter = DynFilterFn::new(|metadata, cx| {
    ///     // If this *is* "interesting_span", make sure to enable it.
    ///     if metadata.is_span() && metadata.name() == "interesting_span" {
    ///         return true;
    ///     }
    ///
    ///     // Otherwise, are we in an interesting span?
    ///     if let Some(current_span) = cx.lookup_current() {
    ///         return current_span.name() == "interesting_span";
    ///     }
    ///
    ///     false
    /// }).with_callsite_filter(|metadata| {
    ///     // If this is an "interesting_span", we know we will always
    ///     // enable it.
    ///     if metadata.is_span() && metadata.name() == "interesting_span" {
    ///         return Interest::always();
    ///     }
    ///
    ///     // Otherwise, it depends on whether or not we're in an interesting
    ///     // span. You'll have to ask us again for each span/event!
    ///     Interest::sometimes()
    /// });
    ///
    /// let my_layer = tracing_subscriber::fmt::layer();
    ///
    /// tracing_subscriber::registry()
    ///     .with(my_layer.with_filter(my_filter))
    ///     .init();
    /// ```
    ///
    /// [cse]: crate::layer::Filter::callsite_enabled
    /// [`enabled`]: crate::layer::Filter::enabled
    /// [`Metadata`]: tracing_core::Metadata
    /// [span context]: crate::layer::Context
    pub fn with_callsite_filter<R2>(self, callsite_enabled: R2) -> DynFilterFn<S, F, R2>
    where
        R2: Fn(&'static Metadata<'static>) -> Interest,
    {
        let register_callsite = Some(callsite_enabled);
        let DynFilterFn {
            enabled,
            max_level_hint,
            _s,
            ..
        } = self;
        DynFilterFn {
            enabled,
            register_callsite,
            max_level_hint,
            _s,
        }
    }

    fn default_callsite_enabled(&self, metadata: &Metadata<'_>) -> Interest {
        // If it's below the configured max level, assume that `enabled` will
        // never enable it...
        if !is_below_max_level(&self.max_level_hint, metadata) {
            debug_assert!(
                !(self.enabled)(metadata, &Context::none()),
                "DynFilterFn<{}> claimed it would only enable {:?} and below, \
                but it enabled metadata with the {:?} level\nmetadata={:#?}",
                type_name::<F>(),
                self.max_level_hint.unwrap(),
                metadata.level(),
                metadata,
            );
            return Interest::never();
        }

        // Otherwise, since this `enabled` function is dynamic and depends on
        // the current context, we don't know whether this span or event will be
        // enabled or not. Ask again every time it's recorded!
        Interest::sometimes()
    }
}

impl<S, F, R> layer::Filter<S> for DynFilterFn<S, F, R>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
    R: Fn(&'static Metadata<'static>) -> Interest,
{
    fn enabled(&self, metadata: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
        let enabled = (self.enabled)(metadata, cx);
        debug_assert!(
            !enabled || is_below_max_level(&self.max_level_hint, metadata),
            "DynFilterFn<{}> claimed it would only enable {:?} and below, \
            but it enabled metadata with the {:?} level\nmetadata={:#?}",
            type_name::<F>(),
            self.max_level_hint.unwrap(),
            metadata.level(),
            metadata,
        );

        enabled
    }

    fn callsite_enabled(&self, metadata: &'static Metadata<'static>) -> Interest {
        let interest = self
            .register_callsite
            .as_ref()
            .map(|callsite_enabled| callsite_enabled(metadata))
            .unwrap_or_else(|| self.default_callsite_enabled(metadata));
        debug_assert!(
            interest.is_never() || is_below_max_level(&self.max_level_hint, metadata),
            "DynFilterFn<{}, {}> claimed it was only interested in {:?} and below, \
            but it enabled metadata with the {:?} level\nmetadata={:#?}",
            type_name::<F>(),
            type_name::<R>(),
            self.max_level_hint.unwrap(),
            metadata.level(),
            metadata,
        );

        interest
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.max_level_hint
    }
}

impl<S, F, R> fmt::Debug for DynFilterFn<S, F, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("DynFilterFn");
        s.field("enabled", &format_args!("{}", type_name::<F>()));
        if self.register_callsite.is_some() {
            s.field(
                "register_callsite",
                &format_args!("Some({})", type_name::<R>()),
            );
        } else {
            s.field("register_callsite", &format_args!("None"));
        }

        s.field("max_level_hint", &self.max_level_hint).finish()
    }
}

impl<S, F, R> Clone for DynFilterFn<S, F, R>
where
    F: Clone,
    R: Clone,
{
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled.clone(),
            register_callsite: self.register_callsite.clone(),
            max_level_hint: self.max_level_hint,
            _s: PhantomData,
        }
    }
}

impl<F, S> From<F> for DynFilterFn<S, F>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
{
    fn from(f: F) -> Self {
        Self::new(f)
    }
}

// === impl FilterId ===

impl FilterId {
    const fn disabled() -> Self {
        Self(u64::MAX)
    }

    /// Returns a `FilterId` that will consider _all_ spans enabled.
    pub(crate) const fn none() -> Self {
        Self(0)
    }

    pub(crate) fn new(id: u8) -> Self {
        assert!(id < 64, "filter IDs may not be greater than 64");
        Self(1 << id as usize)
    }

    /// Combines two `FilterId`s, returning a new `FilterId` that will match a
    /// [`FilterMap`] where the span was disabled by _either_ this `FilterId`
    /// *or* the combined `FilterId`.
    ///
    /// This method is called by [`Context`]s when adding the `FilterId` of a
    /// [`Filtered`] layer to the context.
    ///
    /// This is necessary for cases where we have a tree of nested [`Filtered`]
    /// layers, like this:
    ///
    /// ```text
    /// Filtered {
    ///     filter1,
    ///     Layered {
    ///         layer1,
    ///         Filtered {
    ///              filter2,
    ///              layer2,
    ///         },
    /// }
    /// ```
    ///
    /// We want `layer2` to be affected by both `filter1` _and_ `filter2`.
    /// Without combining `FilterId`s, this works fine when filtering
    /// `on_event`/`new_span`, because the outer `Filtered` layer (`filter1`)
    /// won't call the inner layer's `on_event` or `new_span` callbacks if it
    /// disabled the event/span.
    ///
    /// However, it _doesn't_ work when filtering span lookups and traversals
    /// (e.g. `scope`). This is because the [`Context`] passed to `layer2`
    /// would set its filter ID to the filter ID of `filter2`, and would skip
    /// spans that were disabled by `filter2`. However, what if a span was
    /// disabled by `filter1`? We wouldn't see it in `new_span`, but we _would_
    /// see it in lookups and traversals...which we don't want.
    ///
    /// When a [`Filtered`] layer adds its ID to a [`Context`], it _combines_ it
    /// with any previous filter ID that the context had, rather than replacing
    /// it. That way, `layer2`'s context will check if a span was disabled by
    /// `filter1` _or_ `filter2`. The way we do this, instead of representing
    /// `FilterId`s as a number number that we shift a 1 over by to get a mask,
    /// we just store the actual mask,so we can combine them with a bitwise-OR.
    ///
    /// For example, if we consider the following case (pretending that the
    /// masks are 8 bits instead of 64 just so i don't have to write out a bunch
    /// of extra zeroes):
    ///
    /// - `filter1` has the filter id 1 (`0b0000_0001`)
    /// - `filter2` has the filter id 2 (`0b0000_0010`)
    ///
    /// A span that gets disabled by filter 1 would have the [`FilterMap`] with
    /// bits `0b0000_0001`.
    ///
    /// If the `FilterId` was internally represented as `(bits to shift + 1),
    /// when `layer2`'s [`Context`] checked if it enabled the  span, it would
    /// make the mask `0b0000_0010` (`1 << 1`). That bit would not be set in the
    /// [`FilterMap`], so it would see that it _didn't_ disable  the span. Which
    /// is *true*, it just doesn't reflect the tree-like shape of the actual
    /// subscriber.
    ///
    /// By having the IDs be masks instead of shifts, though, when the
    /// [`Filtered`] with `filter2` gets the [`Context`] with `filter1`'s filter ID,
    /// instead of replacing it, it ors them together:
    ///
    /// ```ignore
    /// 0b0000_0001 | 0b0000_0010 == 0b0000_0011;
    /// ```
    ///
    /// We then test if the span was disabled by  seeing if _any_ bits in the
    /// mask are `1`:
    ///
    /// ```ignore
    /// filtermap & mask != 0;
    /// 0b0000_0001 & 0b0000_0011 != 0;
    /// 0b0000_0001 != 0;
    /// true;
    /// ```
    ///
    /// [`Context`]: crate::layer::Context
    pub(crate) fn and(self, FilterId(other): Self) -> Self {
        // If this mask is disabled, just return the other --- otherwise, we
        // would always see that every span is disabled.
        if self.0 == Self::disabled().0 {
            return Self(other);
        }

        Self(self.0 | other)
    }
}

impl fmt::Debug for FilterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // don't print a giant set of the numbers 0..63 if the filter ID is disabled.
        if self.0 == Self::disabled().0 {
            return f
                .debug_tuple("FilterId")
                .field(&format_args!("DISABLED"))
                .finish();
        }

        if f.alternate() {
            f.debug_struct("FilterId")
                .field("ids", &format_args!("{:?}", FmtBitset(self.0)))
                .field("bits", &format_args!("{:b}", self.0))
                .finish()
        } else {
            f.debug_tuple("FilterId").field(&FmtBitset(self.0)).finish()
        }
    }
}

impl fmt::Binary for FilterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FilterId")
            .field(&format_args!("{:b}", self.0))
            .finish()
    }
}

// === impl FilterMap ===

impl FilterMap {
    pub(crate) fn set(self, FilterId(mask): FilterId, enabled: bool) -> Self {
        if mask == u64::MAX {
            return self;
        }

        if enabled {
            Self {
                bits: self.bits & (!mask),
            }
        } else {
            Self {
                bits: self.bits | mask,
            }
        }
    }

    #[inline]
    pub(crate) fn is_enabled(self, FilterId(mask): FilterId) -> bool {
        self.bits & mask == 0
    }

    #[inline]
    pub(crate) fn any_enabled(self) -> bool {
        self.bits != u64::MAX
    }
}

impl fmt::Debug for FilterMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let alt = f.alternate();
        let mut s = f.debug_struct("FilterMap");
        s.field("disabled_by", &format_args!("{:?}", &FmtBitset(self.bits)));

        if alt {
            s.field("bits", &format_args!("{:b}", self.bits));
        }

        s.finish()
    }
}

impl fmt::Binary for FilterMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilterMap")
            .field("bits", &format_args!("{:b}", self.bits))
            .finish()
    }
}

// === impl FilterState ===

impl FilterState {
    fn new() -> Self {
        Self {
            enabled: Cell::new(FilterMap::default()),
            interest: RefCell::new(None),

            #[cfg(debug_assertions)]
            counters: DebugCounters::default(),
        }
    }

    fn set(&self, filter: FilterId, enabled: bool) {
        #[cfg(debug_assertions)]
        {
            let in_current_pass = self.counters.in_filter_pass.get();
            if in_current_pass == 0 {
                debug_assert_eq!(self.enabled.get(), FilterMap::default());
            }
            self.counters.in_filter_pass.set(in_current_pass + 1);
            debug_assert_eq!(
                self.counters.in_interest_pass.get(),
                0,
                "if we are in or starting a filter pass, we must not be in an interest pass."
            )
        }

        self.enabled.set(self.enabled.get().set(filter, enabled))
    }

    fn add_interest(&self, interest: Interest) {
        let mut curr_interest = self.interest.borrow_mut();

        #[cfg(debug_assertions)]
        {
            let in_current_pass = self.counters.in_interest_pass.get();
            if in_current_pass == 0 {
                debug_assert!(curr_interest.is_none());
            }
            self.counters.in_interest_pass.set(in_current_pass + 1);
        }

        if let Some(curr_interest) = curr_interest.as_mut() {
            if (curr_interest.is_always() && !interest.is_always())
                || (curr_interest.is_never() && !interest.is_never())
            {
                *curr_interest = Interest::sometimes();
            }
            // If the two interests are the same, do nothing. If the current
            // interest is `sometimes`, stay sometimes.
        } else {
            *curr_interest = Some(interest);
        }
    }

    pub(crate) fn event_enabled() -> bool {
        FILTERING
            .try_with(|this| {
                let enabled = this.enabled.get().any_enabled();
                #[cfg(debug_assertions)]
                {
                    if this.counters.in_filter_pass.get() == 0 {
                        debug_assert_eq!(this.enabled.get(), FilterMap::default());
                    }

                    // Nothing enabled this event, we won't tick back down the
                    // counter in `did_enable`. Reset it.
                    if !enabled {
                        this.counters.in_filter_pass.set(0);
                    }
                }
                enabled
            })
            .unwrap_or(true)
    }

    /// Executes a closure if the filter with the provided ID did not disable
    /// the current span/event.
    ///
    /// This is used to implement the `on_event` and `new_span` methods for
    /// `Filtered`.
    fn did_enable(&self, filter: FilterId, f: impl FnOnce()) {
        let map = self.enabled.get();
        if map.is_enabled(filter) {
            // If the filter didn't disable the current span/event, run the
            // callback.
            f();
        } else {
            // Otherwise, if this filter _did_ disable the span or event
            // currently being processed, clear its bit from this thread's
            // `FilterState`. The bit has already been "consumed" by skipping
            // this callback, and we need to ensure that the `FilterMap` for
            // this thread is reset when the *next* `enabled` call occurs.
            self.enabled.set(map.set(filter, true));
        }
        #[cfg(debug_assertions)]
        {
            let in_current_pass = self.counters.in_filter_pass.get();
            if in_current_pass <= 1 {
                debug_assert_eq!(self.enabled.get(), FilterMap::default());
            }
            self.counters
                .in_filter_pass
                .set(in_current_pass.saturating_sub(1));
            debug_assert_eq!(
                self.counters.in_interest_pass.get(),
                0,
                "if we are in a filter pass, we must not be in an interest pass."
            )
        }
    }

    pub(crate) fn take_interest() -> Option<Interest> {
        FILTERING
            .try_with(|filtering| {
                #[cfg(debug_assertions)]
                {
                    if filtering.counters.in_interest_pass.get() == 0 {
                        debug_assert!(filtering.interest.try_borrow().ok()?.is_none());
                    }
                    filtering.counters.in_interest_pass.set(0);
                }
                filtering.interest.try_borrow_mut().ok()?.take()
            })
            .ok()?
    }

    pub(crate) fn filter_map(&self) -> FilterMap {
        let map = self.enabled.get();
        #[cfg(debug_assertions)]
        if self.counters.in_filter_pass.get() == 0 {
            debug_assert_eq!(map, FilterMap::default());
        }

        map
    }
}
/// This is a horrible and bad abuse of the downcasting system to expose
/// *internally* whether a layer has per-layer filtering, within
/// `tracing-subscriber`, without exposing a public API for it.
///
/// If a `Layer` has per-layer filtering, it will downcast to a
/// `MagicPlfDowncastMarker`. Since layers which contain other layers permit
/// downcasting to recurse to their children, this will do the Right Thing with
/// layers like Reload, Option, etc.
///
/// Why is this a wrapper around the `FilterId`, you may ask? Because
/// downcasting works by returning a pointer, and we don't want to risk
/// introducing UB by  constructing pointers that _don't_ point to a valid
/// instance of the type they claim to be. In this case, we don't _intend_ for
/// this pointer to be dereferenced, so it would actually be fine to return one
/// that isn't a valid pointer...but we can't guarantee that the caller won't
/// (accidentally) dereference it, so it's better to be safe than sorry. We
/// could, alternatively, add an additional field to the type that's used only
/// for returning pointers to as as part of the evil downcasting hack, but I
/// thought it was nicer to just add a `repr(transparent)` wrapper to the
/// existing `FilterId` field, since it won't make the struct any bigger.
///
/// Don't worry, this isn't on the test. :)
#[derive(Clone, Copy)]
#[repr(transparent)]
struct MagicPlfDowncastMarker(FilterId);
impl fmt::Debug for MagicPlfDowncastMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Just pretend that `MagicPlfDowncastMarker` doesn't exist for
        // `fmt::Debug` purposes...if no one *sees* it in their `Debug` output,
        // they don't have to know I thought this code would be a good idea.
        fmt::Debug::fmt(&self.0, f)
    }
}

pub(crate) fn is_plf_downcast_marker(type_id: TypeId) -> bool {
    type_id == TypeId::of::<MagicPlfDowncastMarker>()
}

/// Does a type implementing `Subscriber` contain any per-layer filters?
pub(crate) fn subscriber_has_plf<S>(subscriber: &S) -> bool
where
    S: Subscriber,
{
    (subscriber as &dyn Subscriber).is::<MagicPlfDowncastMarker>()
}

/// Does a type implementing `Layer` contain any per-layer filters?
pub(crate) fn layer_has_plf<L, S>(layer: &L) -> bool
where
    L: Layer<S>,
    S: Subscriber,
{
    unsafe {
        // Safety: we're not actually *doing* anything with this pointer --- we
        // only care about the `Option`, which we're turning into a `bool`. So
        // even if the layer decides to be evil and give us some kind of invalid
        // pointer, we don't ever dereference it, so this is always safe.
        layer.downcast_raw(TypeId::of::<MagicPlfDowncastMarker>())
    }
    .is_some()
}

struct FmtBitset(u64);

impl fmt::Debug for FmtBitset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut set = f.debug_set();
        for bit in 0..64 {
            // if the `bit`-th bit is set, add it to the debug set
            if self.0 & (1 << bit) != 0 {
                set.entry(&bit);
            }
        }
        set.finish()
    }
}

fn is_below_max_level(hint: &Option<LevelFilter>, metadata: &Metadata<'_>) -> bool {
    hint.as_ref()
        .map(|hint| metadata.level() <= hint)
        .unwrap_or(true)
}