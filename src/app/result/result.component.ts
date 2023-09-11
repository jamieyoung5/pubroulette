import {ChangeDetectorRef, Component, Injectable, NgZone, OnInit} from '@angular/core';
import {DataSharingService} from "../data-sharing.service";
import {PubRouletteResult} from "./result";
import {GoogleMapComponent} from "../google-map/google-map.component";

@Component({
  selector: 'result',
  templateUrl: './result.component.html'
})
@Injectable({
  providedIn: 'root',
})
export class ResultComponent implements OnInit{
  pubResult: PubRouletteResult;
  startLat: number;
  startLong: number;

  constructor(private ngZone: NgZone, private cdr: ChangeDetectorRef, private dataSharingService: DataSharingService, private googleMapComponent: GoogleMapComponent) {
    this.pubResult = new PubRouletteResult('', '', 0, '');
    this.startLat = 0;
    this.startLong = 0;
  }


  ngOnInit() {
    this.dataSharingService.startLat$.subscribe((result) => {
      this.startLat = result;
    })

    this.dataSharingService.startLong$.subscribe((result) => {
      this.startLong = result;
    })

    this.dataSharingService.pubResult$.subscribe((result) => {
      this.pubResult = result;
      const latLong = this.pubResult.location.split(',');
      this.googleMapComponent.initMap(this.startLat, this.startLong, Number(latLong[1]), Number(latLong[0])
      )
    });
  }

}
