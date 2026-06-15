int classify(int n) {
  switch (n % 3) {
    case 0: return 100;
    case 1: return 200;
    case 2: return 300;
  }
  return -1;
}
int main(void) {
  return classify(7);
}
