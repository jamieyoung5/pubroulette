import { Component } from '@angular/core';
import {FormControl, FormGroup} from '@angular/forms';
import {RouletteService} from "./roulette.service";
import { EventEmitter, Input, Output } from '@angular/core';
import {Router} from "@angular/router";
import {DataSharingService} from "../data-sharing.service";
import {PubRouletteParameters} from "./roulette";
import {PubRouletteLocation, PubRouletteResult} from "../result/result";
import {ResultComponent} from "../result/result.component";

@Component({
  selector: 'roulette',
  templateUrl: './roulette.component.html'
})
export class RouletteComponent {

  location: string = "";
  radius: string = "";
  showResult: boolean = false;
  showResultContainer: boolean = false;
  showErrorMessage: boolean = false;
  errorMessage: string = "";
  loading: boolean = true;

  pubRouletteForm = new FormGroup({
    location: new FormControl(this.location),
    radius: new FormControl(this.radius)
  })

  constructor(public pubRouletteService: RouletteService, public dataSharingService: DataSharingService) {}

  onSubmit() {
    if (this.pubRouletteForm.valid) {
      const radius = parseInt(<string>this.pubRouletteForm.value.radius);
      this.showResultContainer = true;
      this.showResult = false;
      this.showErrorMessage = false;
      this.loading = true;

      let location = this.getPosition()
        .then((result) => {
          this.loading = false;
          this.dataSharingService.setStartLocation(result)
          this.fetchRandomPub(result, radius);
        })
        .catch((error) => {
          console.log('An error occurred: ' + error);
        });

    }else{
      console.log("form is not valid")
    }
  }

  private fetchRandomPub(location: string, radius: number) {
    this.pubRouletteService.getPubs(location, radius).subscribe(
      (response: PubRouletteResult) => {
        this.handlePubResult(response);
      },
      (error) => {
        console.error('Error fetching random pub:', error);
      }
    );
  }

  private handlePubResult(pubResult: PubRouletteResult) {
    if (pubResult.status == "NO_PUBS") {
      this.errorMessage = "No pubs in your radius found ):";
      this.showErrorMessage = true;
    }else if(pubResult.status == "RATE_LIMIT_EXCEEDED") {
      this.errorMessage = "You have exceeded the rate limit! Try again in a few seconds."
      this.showErrorMessage = true;
    }
    else {
      this.dataSharingService.setPubResult(pubResult);
      this.showResult = true
    }
  }


  private getPosition(): Promise<string>
  {
    return new Promise((resolve, reject) => {

      navigator.geolocation.getCurrentPosition(resp => {

          resolve( resp.coords.latitude + "%2C" + resp.coords.longitude  );
        },
        err => {
          reject(err);
        });
    });

  }

}
