int main(void) {
  char *table[3];
  table[0] = "AB";
  table[1] = "CD";
  table[2] = "EF";
  return table[1][0] + table[2][1];
}
