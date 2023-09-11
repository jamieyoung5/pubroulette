// google-map.component.ts
import {Component, Injectable, OnInit} from '@angular/core';

declare var google: any;

@Component({
  selector: 'app-google-map',
  templateUrl: './google-map.component.html',
  styleUrls: ['./google-map.component.css']
})
@Injectable({
  providedIn: 'root',
})
export class GoogleMapComponent {
  map: any;
  directionsService: any;
  directionsRenderer: any;

  initMap(startLat: number, startLong: number, endLat: number, endLong: number): void {
    this.map = new google.maps.Map(document.getElementById('map'), {
      center: { lat: startLat, lng: startLong },
      zoom: 10,
    });

    this.directionsService = new google.maps.DirectionsService();
    this.directionsRenderer = new google.maps.DirectionsRenderer();
    this.directionsRenderer.setMap(this.map);

    this.calculateAndDisplayRoute(startLat, startLong, endLat, endLong)
  }

  calculateAndDisplayRoute(startLat: number, startLong: number, endLat: number, endLong: number): void {
    const start = new google.maps.LatLng(startLat, startLong);
    const end = new google.maps.LatLng(endLat, endLong);

    const request = {
      origin: start,
      destination: end,
      travelMode: google.maps.TravelMode.WALKING,
    };

    this.directionsService.route(request, (result: any, status: any) => {
      if (status === 'OK') {
        this.directionsRenderer.setDirections(result);
      } else {
        console.log('Directions request failed due to ' + status);
      }
    });
  }

}
