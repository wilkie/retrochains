int classify(int x) {
  if (x > 100) {
    if (x > 1000) return 3;
    return 2;
  }
  if (x > 10) return 1;
  return 0;
}
int main(void) {
  return classify(50);
}
