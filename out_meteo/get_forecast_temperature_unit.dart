import 'package:freezed_annotation/freezed_annotation.dart';

enum GetForecastTemperatureUnit {
  @JsonValue('celsius')
  celsius,
  @JsonValue('fahrenheit')
  fahrenheit;
}
