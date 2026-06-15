int classify(int x) {
  switch (x) {
    case 0: return 100;
    case 1: return 200;
    default: return 0;
  }
}
int main(void) {
  return classify(1);
}
