export class PubRouletteLocation {
  longitude: string;
  latitude: string;

  constructor(longitude: string, latitude: string) {
    this.longitude = longitude;
    this.latitude = latitude;
  }
}


export class PubRouletteResult {
  name: string;
  location: string;
  rating: string;
  status: string;

  constructor(name: string, location: string, rating: string, status: string) {
    this.name = name;
    this.location = location;
    this.rating = rating;
    this.status = status;
  }
}
