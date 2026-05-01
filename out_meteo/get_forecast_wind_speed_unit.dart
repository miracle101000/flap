import 'package:freezed_annotation/freezed_annotation.dart';

enum GetForecastWindSpeedUnit {
  @JsonValue('kmh')
  kmh,
  @JsonValue('ms')
  ms,
  @JsonValue('mph')
  mph,
  @JsonValue('kn')
  kn;
}
