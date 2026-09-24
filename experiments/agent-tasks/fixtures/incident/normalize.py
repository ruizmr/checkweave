def region_code(row):
    table = {"us": "US", "eu": "EU"}
    key = str(row["region"]).strip().lower()
    return table[key]
