import 'package:dio/dio.dart';

import 'api_error.dart';
import 'forecast_response.dart';
import 'get_forecast_temperature_unit.dart';
import 'get_forecast_wind_speed_unit.dart';

class OpenMeteoClient {
  OpenMeteoClient({required String baseUrl})
      : _dio = Dio(BaseOptions(baseUrl: baseUrl));

  final Dio _dio;

  /// 7 day weather forecast for coordinates
  // GET /v1/forecast
  Future<ForecastResponse> getForecast({
    required double latitude,
    required double longitude,
    String? current,
    String? daily,
    int? forecastDays,
    String? hourly,
    GetForecastTemperatureUnit? temperatureUnit,
    String? timezone,
    GetForecastWindSpeedUnit? windSpeedUnit,
  }) async {
    final queryParameters = <String, dynamic>{
      'latitude': latitude,
      'longitude': longitude,
      if (hourly != null) 'hourly': hourly,
      if (daily != null) 'daily': daily,
      if (current != null) 'current': current,
      if (timezone != null) 'timezone': timezone,
      if (forecastDays != null) 'forecast_days': forecastDays,
      if (temperatureUnit != null) 'temperature_unit': temperatureUnit,
      if (windSpeedUnit != null) 'wind_speed_unit': windSpeedUnit,
    };
    final response = await _dio.request<dynamic>(
      '/v1/forecast',
      options: Options(method: 'GET'),
      queryParameters: queryParameters,
    );
    final data = response.data;
    return ForecastResponse.fromJson(data as Map<String, dynamic>);
  }
}
