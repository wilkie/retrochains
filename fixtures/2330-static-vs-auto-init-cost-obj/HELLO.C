int sm(void) {
  static int counter = 100;
  counter++;
  return counter;
}
int au(void) {
  int counter = 100;
  counter++;
  return counter;
}
int main(void) {
  return sm() + au();
}
