// data-sharing.service.ts
import { Injectable } from '@angular/core';
import {BehaviorSubject, Observable, Subject} from 'rxjs';
import {PubRouletteResult} from "./result/result";
import {toNumbers} from "@angular/compiler-cli/src/diagnostics/typescript_version";

@Injectable({
  providedIn: 'root',
})
export class DataSharingService {
  private pubResultSource = new Subject<PubRouletteResult>();
  pubResult$ = this.pubResultSource.asObservable();

  private startLatSubject = new BehaviorSubject<number>(0)
  startLat$: Observable<number> = this.startLatSubject.asObservable()

  private startLongSubject = new BehaviorSubject<number>(0)
  startLong$: Observable<number> = this.startLongSubject.asObservable()

  setPubResult(result: PubRouletteResult) {
    this.pubResultSource.next(result);
  }

  setStartLocation(location: string) {

    const parts = location.split('%2C');
    this.startLatSubject.next(Number(decodeURIComponent(parts[0])));
    this.startLongSubject.next(Number(decodeURIComponent(parts[1])));
  }

}
