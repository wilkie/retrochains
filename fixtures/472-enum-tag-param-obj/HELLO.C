enum color { RED, GREEN, BLUE };
int g;
int f(enum color c) {
  g = c;
  return 0;
}
int main(void) {
  f(GREEN);
  return 0;
}
