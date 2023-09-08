import { Injectable } from '@angular/core';
import { HttpClient, HttpHeaders } from '@angular/common/http';
import { Observable } from 'rxjs';
import {PubRouletteResult} from "../result/result";

@Injectable({
  providedIn: 'root',
})
export class RouletteService {
  private apiUrl = 'https://api.pubroulette.com/play';

  constructor(private http: HttpClient) {}

  getPubs(location: string, radius: number): Observable<any> {
    const params = {
      location,
      radius: radius.toString(),
    };

    return this.http.post<PubRouletteResult>(this.apiUrl, params);;
  }
}
