import {ChangeDetectorRef, Component, Injectable, NgZone, OnInit} from '@angular/core';
import {DataSharingService} from "../data-sharing.service";
import {PubRouletteParameters} from "../roulette/roulette";
import {PubRouletteResult} from "./result";
import {GoogleMapComponent} from "../google-map/google-map.component";
import {BigInteger} from "@angular/compiler/src/i18n/big_integer";

@Component({
  selector: 'result',
  templateUrl: './result.component.html'
})
@Injectable({
  providedIn: 'root',
})
export class ResultComponent implements OnInit{
  pubResult: PubRouletteResult = new PubRouletteResult('', '', '', '');
  startLat = 0;
  startLong = 0;

  constructor(private ngZone: NgZone, private cdr: ChangeDetectorRef, private dataSharingService: DataSharingService, private googleMapComponent: GoogleMapComponent) {}


  ngOnInit() {
    this.dataSharingService.startLat$.subscribe((result) => {
      this.startLat = result;
    })

    this.dataSharingService.startLong$.subscribe((result) => {
      this.startLong = result;
    })

    this.dataSharingService.pubResult$.subscribe((result) => {
      this.pubResult = result;
      const parts = this.pubResult.location.split(',');
      this.googleMapComponent.initMap(this.startLat, this.startLong, Number(parts[1]), Number(parts[0])
      )
    });
  }

}
