enum color { RED, GREEN, BLUE };
int main(void) {
  enum color c = GREEN;
  return (int)c * 10 + RED + BLUE;
}
