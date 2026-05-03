// GENERATED — do not edit by hand.
// Shared runtime for fields whose absence and explicit-null forms must
// be distinguished on the wire (notably HTTP PATCH bodies).

import 'package:freezed_annotation/freezed_annotation.dart';

/// Tri-state wrapper. `Optional.absent()` means "the key was omitted
/// from the payload"; `Optional.present(value)` means "the key was
/// supplied with this value", where `value` itself may be `null`.
sealed class Optional<T> {
  const Optional();
  const factory Optional.present(T value) = _Present<T>;
  const factory Optional.absent() = _Absent<T>;

  bool get isPresent => this is _Present<T>;
  bool get isAbsent => this is _Absent<T>;

  /// Throws if `isAbsent`. Use `valueOrNull` for a fallback.
  T get value => switch (this) {
        _Present<T>(:final value) => value,
        _Absent<T>() =>
          throw StateError('Optional.value called on Optional.absent()'),
      };

  T? get valueOrNull => switch (this) {
        _Present<T>(:final value) => value,
        _Absent<T>() => null,
      };
}

final class _Present<T> extends Optional<T> {
  final T value;
  const _Present(this.value);

  @override
  bool operator ==(Object other) =>
      identical(this, other) ||
      (other is _Present<T> && other.value == value);

  @override
  int get hashCode => Object.hash(_Present, value);
}

final class _Absent<T> extends Optional<T> {
  const _Absent();

  @override
  bool operator ==(Object other) => other is _Absent<T>;

  @override
  int get hashCode => (_Absent).hashCode;
}

/// Sentinel emitted by [OptionalConverter.toJson] for the absent case.
/// `stripOptionalAbsent` removes any map entry whose value is identical
/// to this object before the map ever reaches `jsonEncode`.
const Object kOptionalAbsentSentinel = _OptionalAbsentSentinel();

class _OptionalAbsentSentinel {
  const _OptionalAbsentSentinel();
}

/// Removes any entry whose value is the absence sentinel. Generated
/// `toJson` overrides on models with `Optional` fields call this on the
/// `_$ClassNameToJson` output before returning.
Map<String, dynamic> stripOptionalAbsent(Map<String, dynamic> m) {
  m.removeWhere((_, v) => identical(v, kOptionalAbsentSentinel));
  return m;
}

/// Converter for `Optional<T?>` where `T` has a direct JSON shape
/// (`String`, `int`, `double`, `num`, `bool`). For non-primitive `T`
/// (DateTime, custom classes, lists, maps), generated code emits
/// per-field `@JsonKey(fromJson: ..., toJson: ...)` lambdas instead,
/// because `as T?` won't survive the JSON-side runtime types.
///
/// Round-trip semantics:
/// - `fromJson(null)` → `Optional.present(null)` (key was present with null)
/// - `fromJson(value)` → `Optional.present(value)`
/// - **the absent case is encoded by NOT calling fromJson at all**, which
///   relies on `@Default(Optional<T?>.absent())` on the field.
/// - `toJson(Optional.absent())` → sentinel (stripped at the boundary)
/// - `toJson(Optional.present(null))` → `null` (preserved as `"key": null`)
/// - `toJson(Optional.present(value))` → `value`
class OptionalConverter<T> implements JsonConverter<Optional<T?>, Object?> {
  const OptionalConverter();

  @override
  Optional<T?> fromJson(Object? json) => Optional<T?>.present(json as T?);

  @override
  Object? toJson(Optional<T?> opt) => switch (opt) {
        _Absent<T?>() => kOptionalAbsentSentinel,
        _Present<T?>(:final value) => value,
      };
}
