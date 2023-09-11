export class PubRouletteResult {
  name: string;
  location: string;
  rating: number;
  status: string;

  constructor(name: string, location: string, rating: number, status: string) {
    this.name = name;
    this.location = location;
    this.rating = rating
    this.status = status;
  }
}
