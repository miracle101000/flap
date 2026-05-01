import 'package:freezed_annotation/freezed_annotation.dart';

part 'forecast_response.freezed.dart';
part 'forecast_response.g.dart';

@freezed
class ForecastResponse with _$ForecastResponse {
  const factory ForecastResponse({
    Map<String, String>? current,
    @JsonKey(name: 'current_units') Map<String, String>? currentUnits,
    @JsonKey(name: 'daily_units') Map<String, String>? dailyUnits,
    required double elevation,
    @JsonKey(name: 'generationtime_ms') required double generationtimeMs,
    @JsonKey(name: 'hourly_units') Map<String, String>? hourlyUnits,
    required double latitude,
    required double longitude,
    required String timezone,
    @JsonKey(name: 'timezone_abbreviation') required String timezoneAbbreviation,
    @JsonKey(name: 'utc_offset_seconds') required int utcOffsetSeconds,
  }) = _ForecastResponse;

  factory ForecastResponse.fromJson(Map<String, dynamic> json) =>
      _$ForecastResponseFromJson(json);
}
